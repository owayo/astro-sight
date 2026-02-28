use anyhow::Result;
use camino::Utf8Path;
use std::path::Path;

use crate::engine::{calls, diff, parser, refs, symbols};
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
        affected: Vec<AffectedSymbol>,
        sig_changes: Vec<SignatureChange>,
        hunks: Vec<crate::models::impact::HunkInfo>,
        call_edges: Vec<crate::models::call::CallEdge>,
    }

    let mut file_contexts = Vec::new();
    let mut all_symbol_names: Vec<String> = Vec::new();

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

        for sym in &affected {
            if !all_symbol_names.contains(&sym.name) {
                all_symbol_names.push(sym.name.clone());
            }
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

    for ctx in file_contexts {
        let mut impacted_callers = Vec::new();

        // Cross-file callers from batch results (only References, not Definitions)
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
