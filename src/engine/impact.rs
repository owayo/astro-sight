use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use camino::Utf8Path;

use crate::engine::{calls, diff, parser, refs, symbols};
use crate::language::LangId;
use crate::models::call::CallEdge;
use crate::models::impact::{
    AffectedSymbol, ContextResult, DiffFile, FileImpact, HunkInfo, ImpactedCaller, SignatureChange,
};
use crate::models::reference::{RefKind, SymbolReference};
use crate::models::symbol::SymbolKind;

struct FileContext {
    new_path: String,
    lang_id: LangId,
    affected: Vec<AffectedSymbol>,
    sig_changes: Vec<SignatureChange>,
    hunks: Vec<HunkInfo>,
    call_edges: Vec<CallEdge>,
}

/// Analyze the impact of a unified diff within a workspace directory.
///
/// Uses a 2-pass approach for cross-file references:
///   Pass 1: Parse changed files, collect affected symbols per file.
///   Pass 2: Batch-search all affected symbol names in one directory walk.
pub fn analyze_impact(diff_input: &str, dir: &Path) -> Result<ContextResult> {
    let diff_files = diff::parse_unified_diff(diff_input);

    // Pass 1: Parse changed files and collect affected symbols
    let (file_contexts, all_symbol_names, method_parent_types, included_symbols) =
        collect_affected_symbols(diff_input, &diff_files, dir);

    // Pass 2: Batch cross-file reference search (one walk for all symbols)
    let batch_refs = if all_symbol_names.is_empty() {
        HashMap::new()
    } else {
        refs::find_references_batch(&all_symbol_names, dir, None).unwrap_or_default()
    };

    // Pass 3: Assemble results
    let changes = assemble_impacts(
        file_contexts,
        &batch_refs,
        &method_parent_types,
        &included_symbols,
    );

    Ok(ContextResult { changes })
}

/// Pass 1: Parse each changed file, extract symbols, and determine which
/// symbol names need cross-file reference search.
fn collect_affected_symbols(
    diff_input: &str,
    diff_files: &[DiffFile],
    dir: &Path,
) -> (
    Vec<FileContext>,
    Vec<String>,
    HashMap<String, String>,
    HashSet<String>,
) {
    let mut file_contexts = Vec::new();
    let mut all_symbol_names: Vec<String> = Vec::new();
    let mut symbol_name_set: HashSet<String> = HashSet::new();
    let mut method_parent_types: HashMap<String, String> = HashMap::new();
    let mut included_symbols: HashSet<String> = HashSet::new();

    for df in diff_files {
        if !is_safe_diff_path(&df.new_path) {
            continue;
        }

        let file_path = dir.join(&df.new_path);
        if !file_path.exists() {
            continue;
        }

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
        let affected_raw = find_affected_symbols(&syms, &df.hunks);
        // Filter test symbols from affected list so they don't appear in output
        // and don't propagate as impact sources.
        let affected: Vec<AffectedSymbol> = affected_raw
            .into_iter()
            .filter(|sym| {
                !find_overlapping_symbol(&syms, &sym.name, &df.hunks).is_some_and(|s| {
                    is_in_test_context(root, &source, &s.range, lang_id, &df.new_path)
                })
            })
            .collect();
        let sig_changes = detect_signature_changes(diff_input, &df.new_path, &affected);
        let call_edges = calls::extract_calls(root, &source, lang_id, None).unwrap_or_default();

        for sym in &affected {
            if symbol_name_set.contains(&sym.name) {
                continue;
            }
            if !should_include_for_cross_file(
                sym,
                &syms,
                &df.hunks,
                &sig_changes,
                diff_input,
                &df.new_path,
                root,
                &source,
                lang_id,
            ) {
                continue;
            }
            included_symbols.insert(sym.name.clone());
            if let Some(orig) = find_overlapping_symbol(&syms, &sym.name, &df.hunks)
                && let Some(parent_type) =
                    find_parent_type_name(root, &source, &orig.range, lang_id)
            {
                method_parent_types.insert(sym.name.clone(), parent_type.clone());
                if symbol_name_set.insert(parent_type.clone()) {
                    all_symbol_names.push(parent_type);
                }
            }
            symbol_name_set.insert(sym.name.clone());
            all_symbol_names.push(sym.name.clone());
        }

        let hunks = df
            .hunks
            .iter()
            .map(|h| HunkInfo {
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

    (
        file_contexts,
        all_symbol_names,
        method_parent_types,
        included_symbols,
    )
}

/// Pass 3: For each changed file, collect cross-file and same-file impacted callers.
///
/// Only symbols that passed `should_include_for_cross_file` (tracked in `included_symbols`)
/// are used for cross-file reference lookup. Symbols added to `batch_refs` solely as parent
/// types for method scoping are not iterated as impact sources.
fn assemble_impacts(
    file_contexts: Vec<FileContext>,
    batch_refs: &HashMap<String, Vec<SymbolReference>>,
    method_parent_types: &HashMap<String, String>,
    included_symbols: &HashSet<String>,
) -> Vec<FileImpact> {
    let mut changes = Vec::new();
    let mut target_file_cache: HashMap<String, Option<ParsedFile>> = HashMap::new();

    for ctx in file_contexts {
        // (path, line) → (name, symbols) で重複マージしつつシンボルを追跡
        let mut caller_map: HashMap<(String, usize), (String, Vec<String>)> = HashMap::new();

        let source_lang_group = lang_compat_group(ctx.lang_id);
        for sym in &ctx.affected {
            if !included_symbols.contains(&sym.name) {
                continue;
            }
            if let Some(caller_refs) = batch_refs.get(&sym.name) {
                for r in caller_refs {
                    if !is_relevant_cross_file_ref(
                        r,
                        &ctx.new_path,
                        source_lang_group,
                        &sym.name,
                        method_parent_types,
                        batch_refs,
                        &mut target_file_cache,
                    ) {
                        continue;
                    }
                    let key = (r.path.clone(), r.line);
                    let entry = caller_map.entry(key).or_insert_with(|| {
                        let name = r
                            .context
                            .as_deref()
                            .and_then(extract_function_from_context)
                            .unwrap_or_else(|| sym.name.clone());
                        (name, Vec::new())
                    });
                    if !entry.1.contains(&sym.name) {
                        entry.1.push(sym.name.clone());
                    }
                }
            }
        }

        for sym in &ctx.affected {
            for edge in &ctx.call_edges {
                if edge.callee.name == sym.name {
                    let caller_line = edge.call_site.line;
                    if !ctx.affected.iter().any(|a| a.name == edge.caller.name) {
                        let key = (ctx.new_path.clone(), caller_line);
                        let entry = caller_map
                            .entry(key)
                            .or_insert_with(|| (edge.caller.name.clone(), Vec::new()));
                        if !entry.1.contains(&sym.name) {
                            entry.1.push(sym.name.clone());
                        }
                    }
                }
            }
        }

        let mut impacted_callers: Vec<ImpactedCaller> = caller_map
            .into_iter()
            .map(|((path, line), (name, symbols))| ImpactedCaller {
                path,
                name,
                line,
                symbols,
            })
            .collect();
        impacted_callers.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));

        changes.push(FileImpact {
            path: ctx.new_path,
            hunks: ctx.hunks,
            affected_symbols: ctx.affected,
            signature_changes: ctx.sig_changes,
            impacted_callers,
        });
    }

    changes
}

/// Determine whether an affected symbol should be included in cross-file reference search.
///
/// Applies a 5-stage filter:
/// 1. Skip impl block type names (not API-affecting)
/// 2. Skip symbols in test context
/// 3. Skip functions/methods with body-only changes (no signature change)
/// 4. Skip non-exported symbols
/// 5. Skip symbols whose name doesn't appear in any changed diff line
#[allow(clippy::too_many_arguments)]
fn should_include_for_cross_file(
    sym: &AffectedSymbol,
    syms: &[crate::models::symbol::Symbol],
    hunks: &[HunkInfo],
    sig_changes: &[SignatureChange],
    diff_input: &str,
    file_path: &str,
    root: tree_sitter::Node,
    source: &[u8],
    lang_id: LangId,
) -> bool {
    // 1. Skip impl block type names and module declarations
    // Module declarations (e.g. `pub mod tensor`) don't change API surface;
    // actual content changes are detected from the module's own files in the diff.
    if sym.kind == "type" || sym.kind == "module" {
        return false;
    }
    // 2. Skip symbols in test context
    if find_overlapping_symbol(syms, &sym.name, hunks)
        .is_some_and(|s| is_in_test_context(root, source, &s.range, lang_id, file_path))
    {
        return false;
    }
    // 3. Skip functions/methods with body-only changes
    if (sym.kind == "function" || sym.kind == "method")
        && !sig_changes.iter().any(|sc| sc.name == sym.name)
    {
        return false;
    }
    // 3b. Skip type-like symbols whose definition header is unchanged.
    // e.g. if `trait GuestMemory` line itself didn't change, don't propagate
    // even if the name appears in other changed lines (like free function signatures).
    if matches!(
        sym.kind.as_str(),
        "trait" | "struct" | "class" | "interface" | "enum"
    ) && !is_definition_header_in_changed_lines(diff_input, file_path, &sym.name, &sym.kind)
    {
        return false;
    }
    // 4. Skip non-exported symbols
    if !find_overlapping_symbol(syms, &sym.name, hunks)
        .is_some_and(|s| symbols::is_symbol_exported(root, source, lang_id, &s.range))
    {
        return false;
    }
    // 5. Skip if symbol name doesn't appear in any changed line
    if !is_symbol_in_changed_lines(diff_input, file_path, &sym.name) {
        return false;
    }
    true
}

/// Determine whether a cross-file reference is relevant as an impacted caller.
///
/// Applies a 5-stage filter:
/// 1. Skip definitions (only call-site references matter)
/// 2. Skip same-file refs
/// 3. Skip cross-language false positives
/// 4. Skip refs lacking parent type in the target file (method type scoping)
/// 5. Skip refs in test context in the target file
fn is_relevant_cross_file_ref(
    r: &SymbolReference,
    source_path: &str,
    source_lang_group: u8,
    sym_name: &str,
    method_parent_types: &HashMap<String, String>,
    batch_refs: &HashMap<String, Vec<SymbolReference>>,
    target_file_cache: &mut HashMap<String, Option<ParsedFile>>,
) -> bool {
    // 1. Skip definitions
    if r.kind == Some(RefKind::Definition) {
        return false;
    }
    // 2. Skip same-file refs
    if r.path.ends_with(source_path) {
        return false;
    }
    // 3. Skip cross-language false positives
    if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(&r.path))
        && lang_compat_group(ref_lang) != source_lang_group
    {
        return false;
    }
    // 4. Method type scoping
    if let Some(parent_type) = method_parent_types.get(sym_name) {
        let type_in_ref_file = batch_refs
            .get(parent_type.as_str())
            .is_some_and(|type_refs| type_refs.iter().any(|tr| tr.path == r.path));
        if !type_in_ref_file {
            return false;
        }
        // 4b. 同名メソッドの型横断マッチ防止
        // ソースファイル以外に同名メソッドの Definition が存在する場合、
        // 異なる型が同名メソッドを定義している。名前だけでは呼び出し元が
        // どの型のメソッドを参照しているか判別できないため保守的にフィルタ。
        // (例: MmapDictionary::search と DoubleArrayTrie::search が共存する場合、
        //  ターゲットファイルの search() 呼び出しがどちらの型かは不明)
        if let Some(all_refs) = batch_refs.get(sym_name) {
            let has_competing_def = all_refs.iter().any(|other| {
                other.kind == Some(RefKind::Definition) && !other.path.ends_with(source_path)
            });
            if has_competing_def {
                return false;
            }
        }
    }
    // 5. Skip refs in test context
    if is_ref_in_target_test_context(&r.path, r.line, r.column, target_file_cache) {
        return false;
    }
    true
}

/// Match hunks against symbol ranges to find affected symbols.
fn find_affected_symbols(
    syms: &[crate::models::symbol::Symbol],
    hunks: &[HunkInfo],
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
                    kind: symbol_kind_str(sym.kind).to_string(),
                    change_type: change_type.to_string(),
                });
                break; // Don't double-count
            }
        }
    }

    affected
}

/// Check if a symbol's range overlaps with any of the given hunks.
fn symbol_overlaps_hunks(sym: &crate::models::symbol::Symbol, hunks: &[HunkInfo]) -> bool {
    hunks.iter().any(|h| {
        let hunk_start = h.new_start.saturating_sub(1);
        let hunk_end = hunk_start + h.new_count;
        hunk_start < sym.range.end.line && hunk_end > sym.range.start.line
    })
}

/// Find the first symbol with the given name that overlaps any hunk.
fn find_overlapping_symbol<'a>(
    syms: &'a [crate::models::symbol::Symbol],
    name: &str,
    hunks: &[HunkInfo],
) -> Option<&'a crate::models::symbol::Symbol> {
    syms.iter()
        .find(|s| s.name == name && symbol_overlaps_hunks(s, hunks))
}

/// Find the deepest AST node covering the given source range.
fn descendant_for_range<'a>(
    root: tree_sitter::Node<'a>,
    range: &crate::models::location::Range,
) -> Option<tree_sitter::Node<'a>> {
    let start = tree_sitter::Point {
        row: range.start.line,
        column: range.start.column,
    };
    let end = tree_sitter::Point {
        row: range.end.line,
        column: range.end.column,
    };
    root.descendant_for_point_range(start, end)
}

fn symbol_kind_str(kind: SymbolKind) -> &'static str {
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

/// Check if a type-like symbol's definition header appears in changed (+/-) lines.
///
/// For trait/struct/class/interface/enum symbols, checks that the declaration keyword
/// followed by the symbol name (e.g. `trait GuestMemory`, `struct Foo`) is present
/// in a changed line. This prevents false positives where the symbol name only appears
/// in other changed symbols' signatures (e.g. `fn read_obj(m: &impl GuestMemory)`).
fn is_definition_header_in_changed_lines(
    diff_input: &str,
    file_path: &str,
    symbol_name: &str,
    kind: &str,
) -> bool {
    let keywords: &[&str] = match kind {
        "trait" => &["trait"],
        "struct" => &["struct"],
        "class" => &["class"],
        "interface" => &["interface", "trait"],
        "enum" => &["enum"],
        _ => return true, // non-type symbols: always pass
    };

    let mut in_file = false;
    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if in_file
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---")))
        {
            let content = &line[1..];
            for kw in keywords {
                let pattern = format!("{kw} {symbol_name}");
                if content.contains(&pattern) {
                    return true;
                }
            }
        }
    }

    false
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
    cache: &mut HashMap<String, Option<ParsedFile>>,
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

    is_in_test_context(tree.root_node(), source, &range, *lang_id, path)
}

/// Check if a symbol is inside a test context.
///
/// Test symbols should not propagate cross-file impacts because:
/// - Test functions are not called from production code
/// - Changes to test helpers only affect the test module
///
/// Uses a two-layer approach:
/// 1. File-path based detection (fast, covers all languages)
/// 2. AST-based detection (precise, language-specific)
fn is_in_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
    lang_id: LangId,
    path: &str,
) -> bool {
    // ファイルパスベースの判定（全言語共通）
    if is_test_file_path(path) {
        return true;
    }
    // AST ベースの判定（言語別）
    match lang_id {
        LangId::Rust => is_rust_test_context(root, source, symbol_range),
        LangId::Python => is_python_test_context(root, source, symbol_range),
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            is_js_test_context(root, source, symbol_range)
        }
        LangId::Java | LangId::Kotlin => is_jvm_test_context(root, source, symbol_range),
        LangId::CSharp => is_csharp_test_context(root, source, symbol_range),
        LangId::Ruby => is_ruby_test_context(root, source, symbol_range),
        _ => false,
    }
}

/// ファイルパスからテストファイルかどうかを判定する。
fn is_test_file_path(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let filename_lower = filename.to_lowercase();

    // Go: *_test.go
    if filename_lower.ends_with("_test.go") {
        return true;
    }
    // JS/TS: *.test.ts, *.spec.ts, *.test.js, *.spec.js, *.test.tsx, *.spec.tsx
    if filename_lower.contains(".test.") || filename_lower.contains(".spec.") {
        return true;
    }
    // Python: test_*.py, *_test.py, conftest.py
    if filename_lower.ends_with(".py")
        && (filename_lower.starts_with("test_")
            || filename_lower.ends_with("_test.py")
            || filename_lower == "conftest.py")
    {
        return true;
    }
    // Java/Kotlin: *Test.java, *Tests.java, *Test.kt, *Tests.kt
    if filename.ends_with("Test.java")
        || filename.ends_with("Tests.java")
        || filename.ends_with("Test.kt")
        || filename.ends_with("Tests.kt")
    {
        return true;
    }
    // C#: *Test.cs, *Tests.cs
    if filename.ends_with("Test.cs") || filename.ends_with("Tests.cs") {
        return true;
    }
    // Ruby: *_test.rb, *_spec.rb
    if filename_lower.ends_with("_test.rb") || filename_lower.ends_with("_spec.rb") {
        return true;
    }
    // PHP: *Test.php, *Tests.php
    if filename.ends_with("Test.php") || filename.ends_with("Tests.php") {
        return true;
    }
    // C/Cpp: *_test.c, *_test.cpp, *_test.cc
    if filename_lower.ends_with("_test.c")
        || filename_lower.ends_with("_test.cpp")
        || filename_lower.ends_with("_test.cc")
    {
        return true;
    }
    // Bash: *.bats, *_test.sh
    if filename_lower.ends_with(".bats") || filename_lower.ends_with("_test.sh") {
        return true;
    }
    false
}

/// Rust: `#[test]` attribute / `#[cfg(test)]` module
fn is_rust_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "function_item" && has_attribute_text(n, source, "test") {
            return true;
        }
        if n.kind() == "mod_item" && has_attribute_text(n, source, "cfg(test)") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Python: `test_*` 関数名 / `TestCase` サブクラス内
fn is_python_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "function_definition"
            && let Some(name) = n.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
            && text.starts_with("test_")
        {
            return true;
        }
        // TestCase サブクラス内
        if n.kind() == "class_definition"
            && let Some(bases) = n.child_by_field_name("superclasses")
            && let Ok(text) = bases.utf8_text(source)
            && text.contains("TestCase")
        {
            return true;
        }
        // pytest decorator
        if n.kind() == "decorated_definition" {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "decorator"
                    && let Ok(text) = child.utf8_text(source)
                    && (text.contains("pytest.fixture") || text.contains("pytest.mark"))
                {
                    return true;
                }
            }
        }
        current = n.parent();
    }
    false
}

/// JS/TS/TSX: `test()`/`it()`/`describe()` call 内
fn is_js_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "call_expression"
            && let Some(func) = n.child_by_field_name("function")
            && let Ok(name) = func.utf8_text(source)
            && matches!(
                name,
                "test" | "it" | "describe" | "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
            )
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin: `@Test` アノテーション
fn is_jvm_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        let is_method = matches!(
            n.kind(),
            "method_declaration" | "function_declaration" | "constructor_declaration"
        );
        if is_method && has_jvm_annotation(n, source, "Test") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin のアノテーション（`@Test` 等）の存在チェック。
fn has_jvm_annotation(node: tree_sitter::Node, source: &[u8], name: &str) -> bool {
    // Java: modifiers > marker_annotation > identifier
    // Kotlin: modifiers > annotation > ... > simple_identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "modifiers" | "marker_annotation" | "annotation"
        ) && let Ok(text) = child.utf8_text(source)
            && text.contains(name)
        {
            return true;
        }
    }
    // prev sibling check (annotation が method の前に独立ノードとして存在する場合)
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if matches!(p.kind(), "marker_annotation" | "annotation") {
            if let Ok(text) = p.utf8_text(source)
                && text.contains(name)
            {
                return true;
            }
            prev = p.prev_named_sibling();
        } else if p.kind().contains("comment") {
            prev = p.prev_named_sibling();
        } else {
            break;
        }
    }
    false
}

/// C#: `[Test]`/`[Fact]`/`[TestMethod]` アトリビュート
fn is_csharp_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "method_declaration"
            && has_csharp_attribute(n, source, &["Test", "TestMethod", "Fact", "Theory"])
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// C# のアトリビュート（`[Test]` 等）の存在チェック。
fn has_csharp_attribute(node: tree_sitter::Node, source: &[u8], names: &[&str]) -> bool {
    // attribute_list が method_declaration の子ノードとして存在
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list"
            && let Ok(text) = child.utf8_text(source)
        {
            for name in names {
                if text.contains(name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Ruby: `describe`/`it` call 内 / `test_*` メソッド名
fn is_ruby_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        // RSpec: describe/it/before/after call
        if n.kind() == "call"
            && let Some(method) = n.child_by_field_name("method")
            && let Ok(name) = method.utf8_text(source)
            && matches!(name, "describe" | "context" | "it" | "before" | "after")
        {
            return true;
        }
        // Minitest: test_* メソッド
        if n.kind() == "method"
            && let Some(name) = n.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
            && (text.starts_with("test_") || text == "setup" || text == "teardown")
        {
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
    let node = descendant_for_range(root, symbol_range)?;

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
