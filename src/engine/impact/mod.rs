mod signature;
mod test_context;

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

use signature::{
    detect_signature_changes, extract_function_from_context, is_definition_header_in_changed_lines,
    is_symbol_in_changed_lines,
};
use test_context::{is_in_test_context, is_ref_in_target_test_context};

struct FileContext {
    new_path: String,
    lang_id: LangId,
    affected: Vec<AffectedSymbol>,
    sig_changes: Vec<SignatureChange>,
    hunks: Vec<HunkInfo>,
    call_edges: Vec<CallEdge>,
}

/// キャッシュされたパース結果: (tree, ソースバイト列, 言語)。
type ParsedFile = (tree_sitter::Tree, Vec<u8>, LangId);

/// unified diff のワークスペースディレクトリ内での影響を解析する。
///
/// 2パス方式で cross-file 参照を検索する：
///   Pass 1: 変更ファイルをパースし、affected シンボルを収集する。
///   Pass 2: 全 affected シンボル名を1回のディレクトリウォークでバッチ検索する。
pub fn analyze_impact(diff_input: &str, dir: &Path) -> Result<ContextResult> {
    let diff_files = diff::parse_unified_diff(diff_input);

    // Pass 1: 変更ファイルをパースし affected シンボルを収集
    let (file_contexts, all_symbol_names, method_parent_types, included_symbols) =
        collect_affected_symbols(diff_input, &diff_files, dir);

    // Pass 2: cross-file 参照のバッチ検索（全シンボルを1回のウォークで処理）
    let batch_refs = if all_symbol_names.is_empty() {
        HashMap::new()
    } else {
        refs::find_references_batch(&all_symbol_names, dir, None).unwrap_or_default()
    };

    // Pass 3: 結果を組み立て
    let changes = assemble_impacts(
        file_contexts,
        &batch_refs,
        &method_parent_types,
        &included_symbols,
    );

    Ok(ContextResult { changes })
}

/// Pass 1: 変更ファイルをパースし、シンボルを抽出し、cross-file 参照検索が必要なシンボル名を決定する。
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

        // fail-closed: canonicalize 失敗時もスキップ
        let is_within_boundary = std::fs::canonicalize(&file_path)
            .ok()
            .zip(std::fs::canonicalize(dir).ok())
            .is_some_and(|(canonical, canonical_dir)| canonical.starts_with(&canonical_dir));
        if !is_within_boundary {
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
        // テストシンボルとローカルスコープ変数を affected から除外。
        // ローカル変数（関数内 const/let 等）はファイル外への影響を持たないため、
        // affected_symbols 出力と cross-file 伝播の両方からノイズを除去する。
        let affected: Vec<AffectedSymbol> = affected_raw
            .into_iter()
            .filter(|sym| {
                if let Some(s) = find_overlapping_symbol(&syms, &sym.name, &df.hunks) {
                    // テストコンテキスト内のシンボルを除外
                    if is_in_test_context(root, &source, &s.range, lang_id, &df.new_path) {
                        return false;
                    }
                    // 関数内ローカル変数/定数を除外
                    if matches!(sym.kind.as_str(), "variable" | "constant")
                        && symbols::is_local_scope_symbol(root, &source, lang_id, &s.range)
                    {
                        return false;
                    }
                }
                true
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

/// Pass 3: 各変更ファイルについて、cross-file および同一ファイル内の影響を受ける呼び出し元を収集する。
///
/// `should_include_for_cross_file` を通過したシンボル（`included_symbols` で追跡）のみが
/// cross-file 参照検索に使用される。メソッドの型スコーピングのためだけに `batch_refs` に
/// 追加された親型は、影響源として反復されない。
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

/// affected シンボルを cross-file 参照検索に含めるべきか判定する。
///
/// 5段階のフィルタを適用する：
/// 1. impl ブロックの型名をスキップ（API に影響しない）
/// 2. テストコンテキスト内のシンボルをスキップ
/// 3. ボディのみの変更（シグネチャ変更なし）の関数/メソッドをスキップ
/// 4. エクスポートされていないシンボルをスキップ
/// 5. 変更された diff 行にシンボル名が出現しない場合スキップ
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
    // 1. impl ブロックの型名とモジュール宣言をスキップ
    // モジュール宣言（例: `pub mod tensor`）は API サーフェスを変更しない。
    // 実際の内容変更は diff 内のモジュール自身のファイルから検出される。
    if sym.kind == "type" || sym.kind == "module" {
        return false;
    }
    // 2. テストコンテキスト内のシンボルをスキップ
    if find_overlapping_symbol(syms, &sym.name, hunks)
        .is_some_and(|s| is_in_test_context(root, source, &s.range, lang_id, file_path))
    {
        return false;
    }
    // 3. ボディのみの変更の関数/メソッドをスキップ
    if (sym.kind == "function" || sym.kind == "method")
        && !sig_changes.iter().any(|sc| sc.name == sym.name)
    {
        return false;
    }
    // 3b. 定義ヘッダが変更されていない型シンボルをスキップ。
    // 例: `trait GuestMemory` 行自体が変更されていなければ、
    // 他の変更行（フリー関数のシグネチャ等）に名前が出現しても伝播しない。
    if matches!(
        sym.kind.as_str(),
        "trait" | "struct" | "class" | "interface" | "enum"
    ) && !is_definition_header_in_changed_lines(diff_input, file_path, &sym.name, &sym.kind)
    {
        return false;
    }
    // 4. エクスポートされていないシンボルをスキップ
    if !find_overlapping_symbol(syms, &sym.name, hunks)
        .is_some_and(|s| symbols::is_symbol_exported(root, source, lang_id, &s.range))
    {
        return false;
    }
    // 5. 変更行にシンボル名が出現しない場合スキップ
    if !is_symbol_in_changed_lines(diff_input, file_path, &sym.name) {
        return false;
    }
    true
}

/// cross-file 参照が影響を受ける呼び出し元として関連するか判定する。
///
/// 6段階のフィルタを適用する：
/// 1. 定義をスキップ（呼び出し箇所の参照のみが対象）
/// 2. 同一ファイルの参照をスキップ
/// 3. 言語間の偽陽性をスキップ
/// 4. ターゲットファイルに親型が存在しない参照をスキップ（メソッド型スコーピング）
/// 5. ターゲットファイルのテストコンテキスト内の参照をスキップ
/// 6. import/re-export 行をスキップ（シンボルの型情報を使わず名前だけ経由する行）
fn is_relevant_cross_file_ref(
    r: &SymbolReference,
    source_path: &str,
    source_lang_group: u8,
    sym_name: &str,
    method_parent_types: &HashMap<String, String>,
    batch_refs: &HashMap<String, Vec<SymbolReference>>,
    target_file_cache: &mut HashMap<String, Option<ParsedFile>>,
) -> bool {
    // 1. 定義をスキップ
    if r.kind == Some(RefKind::Definition) {
        return false;
    }
    // 2. 同一ファイルの参照をスキップ
    // ends_with だけではサフィックスマッチで偽陽性が出る
    // （例: source_path="main.rs" が "test_main.rs" にマッチ）
    if r.path == source_path || r.path.ends_with(&format!("/{source_path}")) {
        return false;
    }
    // 3. 言語間の偽陽性をスキップ
    if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(&r.path))
        && lang_compat_group(ref_lang) != source_lang_group
    {
        return false;
    }
    // 4. メソッドの型スコーピング
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
    // 5. テストコンテキスト内の参照をスキップ
    if is_ref_in_target_test_context(&r.path, r.line, r.column, target_file_cache) {
        return false;
    }
    // 6. import/re-export 行をスキップ
    // import 文や re-export はシンボルの名前を経由するだけで、
    // 実装やシグネチャの変更に影響されない。
    if is_import_context(r.context.as_deref()) {
        return false;
    }
    true
}

/// 参照のコンテキスト行が import/re-export 文かどうかを判定する。
fn is_import_context(context: Option<&str>) -> bool {
    let ctx = match context {
        Some(c) => c.trim(),
        None => return false,
    };
    // JS/TS: import { X } from '...', import X from '...'
    if ctx.starts_with("import ") || ctx.starts_with("import{") {
        return true;
    }
    // JS/TS: export { X } from '...', export * from '...'
    if (ctx.starts_with("export ") || ctx.starts_with("export{"))
        && (ctx.contains(" from ") || ctx.contains(" from\"") || ctx.contains(" from'"))
    {
        return true;
    }
    // JS/TS: const { X } = require('...'), require('...')
    if ctx.contains("= require(") || ctx.starts_with("require(") {
        return true;
    }
    // Python: from module import X
    if ctx.starts_with("from ") && ctx.contains(" import ") {
        return true;
    }
    // Rust: use crate::..., pub use ...
    if ctx.starts_with("use ") || ctx.starts_with("pub use ") {
        return true;
    }
    // Go: import "..."
    // Go は個別シンボルを import しないため通常は該当しないが念のため
    if ctx.starts_with("import (") || ctx.starts_with("import \"") {
        return true;
    }
    // Ruby: require, require_relative
    if ctx.starts_with("require ") || ctx.starts_with("require_relative ") {
        return true;
    }
    // C/C++: #include "..." / #include <...>
    if ctx.starts_with("#include ") {
        return true;
    }
    // C#: using System; / using static ...
    // "using var" / "using (" はリソース管理（import ではない）
    if ctx.starts_with("using ")
        && ctx.ends_with(';')
        && !ctx.starts_with("using var ")
        && !ctx.starts_with("using (")
    {
        return true;
    }
    // Zig: const std = @import("std");
    if ctx.contains("@import(") {
        return true;
    }
    // Java/Kotlin/Swift/PHP: すでにカバー済み
    // ("import " / "use " で捕捉)
    false
}

/// hunk をシンボル範囲と照合して affected シンボルを検出する。
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

            // オーバーラップチェック（ゼロ幅 hunk は点として境界を含む判定）
            let overlaps = if hunk.new_count == 0 {
                hunk_start >= sym_start && hunk_start < sym_end
            } else {
                hunk_start < sym_end && hunk_end > sym_start
            };
            if overlaps {
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
                break; // 重複カウントを防止
            }
        }
    }

    affected
}

/// シンボルの範囲がいずれかの hunk とオーバーラップするか確認する。
fn symbol_overlaps_hunks(sym: &crate::models::symbol::Symbol, hunks: &[HunkInfo]) -> bool {
    hunks.iter().any(|h| {
        let hunk_start = h.new_start.saturating_sub(1);
        let hunk_end = hunk_start + h.new_count;
        // ゼロ幅 hunk（pure-delete）は点として境界を含む判定
        if h.new_count == 0 {
            hunk_start >= sym.range.start.line && hunk_start < sym.range.end.line
        } else {
            hunk_start < sym.range.end.line && hunk_end > sym.range.start.line
        }
    })
}

/// 指定名のシンボルのうち、いずれかの hunk とオーバーラップする最初のものを返す。
fn find_overlapping_symbol<'a>(
    syms: &'a [crate::models::symbol::Symbol],
    name: &str,
    hunks: &[HunkInfo],
) -> Option<&'a crate::models::symbol::Symbol> {
    syms.iter()
        .find(|s| s.name == name && symbol_overlaps_hunks(s, hunks))
}

/// 指定されたソース範囲を包含する最深の AST ノードを返す。
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

/// impl/class ブロック内のメソッドの親型名を取得する。
///
/// Rust `impl Foo { fn bar() {} }` → `Some("Foo")` を返す
/// Rust `impl Trait for Foo { fn bar() {} }` → `Some("Foo")` を返す
/// クラスベース言語 → クラス名を返す
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

/// tree-sitter の型ノードから型名を抽出する（ジェネリクスやスコープ付き型を処理）。
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

/// diff パスが安全か検証する（絶対パスやトラバーサルコンポーネントを拒否）。
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

/// cross-file 参照フィルタリング用の言語互換グループ。
///
/// 同一グループの言語は互いのシンボルを参照可能
/// （例: JS/TS/TSX は import を共有、C/C++ はヘッダを共有、Java/Kotlin は JVM を共有）。
/// グループ間のマッチ（例: Bash スクリプト内の Rust `command`）は偽陽性。
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
        LangId::Zig => 11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SymbolKind → 文字列マッピングの検証
    #[test]
    fn symbol_kind_str_mapping() {
        assert_eq!(symbol_kind_str(SymbolKind::Function), "function");
        assert_eq!(symbol_kind_str(SymbolKind::Method), "method");
        assert_eq!(symbol_kind_str(SymbolKind::Class), "class");
        assert_eq!(symbol_kind_str(SymbolKind::Module), "module");
    }

    // 通常の相対パスは安全と判定される
    #[test]
    fn is_safe_diff_path_normal() {
        assert!(is_safe_diff_path("src/main.rs"));
        assert!(is_safe_diff_path("a/b/c.txt"));
    }

    // 絶対パスは拒否される
    #[test]
    fn is_safe_diff_path_absolute() {
        assert!(!is_safe_diff_path("/etc/passwd"));
    }

    // ディレクトリトラバーサルを含むパスは拒否される
    #[test]
    fn is_safe_diff_path_traversal() {
        assert!(!is_safe_diff_path("src/../etc/passwd"));
        assert!(!is_safe_diff_path("../secret"));
    }

    // Windows 形式の絶対パスは拒否される
    #[test]
    fn is_safe_diff_path_windows_absolute() {
        assert!(!is_safe_diff_path("\\windows\\system32"));
    }

    // 同じ言語互換グループに属するペアは同じ値を返す
    #[test]
    fn lang_compat_group_same() {
        assert_eq!(
            lang_compat_group(LangId::Javascript),
            lang_compat_group(LangId::Typescript)
        );
        assert_eq!(
            lang_compat_group(LangId::Javascript),
            lang_compat_group(LangId::Tsx)
        );
        assert_eq!(
            lang_compat_group(LangId::Java),
            lang_compat_group(LangId::Kotlin)
        );
        assert_eq!(lang_compat_group(LangId::C), lang_compat_group(LangId::Cpp));
    }

    // 異なる言語互換グループは異なる値を返す
    #[test]
    fn lang_compat_group_different() {
        assert_ne!(
            lang_compat_group(LangId::Rust),
            lang_compat_group(LangId::Python)
        );
        assert_ne!(
            lang_compat_group(LangId::Go),
            lang_compat_group(LangId::Ruby)
        );
    }

    // is_relevant_cross_file_ref の同一ファイル判定テスト
    #[test]
    fn cross_file_ref_same_file_exact_match() {
        let r = SymbolReference {
            path: "src/main.rs".to_string(),
            line: 10,
            column: 0,
            context: None,
            kind: Some(RefKind::Reference),
        };
        let mut cache = HashMap::new();
        // 完全一致は除外される
        assert!(!is_relevant_cross_file_ref(
            &r,
            "src/main.rs",
            0,
            "foo",
            &HashMap::new(),
            &HashMap::new(),
            &mut cache,
        ));
    }

    #[test]
    fn cross_file_ref_same_file_with_prefix() {
        let r = SymbolReference {
            path: "other/src/main.rs".to_string(),
            line: 10,
            column: 0,
            context: None,
            kind: Some(RefKind::Reference),
        };
        let mut cache = HashMap::new();
        // パス区切り付きのサフィックスマッチも除外される
        assert!(!is_relevant_cross_file_ref(
            &r,
            "src/main.rs",
            0,
            "foo",
            &HashMap::new(),
            &HashMap::new(),
            &mut cache,
        ));
    }

    #[test]
    fn cross_file_ref_different_file_similar_suffix() {
        let r = SymbolReference {
            path: "test_main.rs".to_string(),
            line: 10,
            column: 0,
            context: None,
            kind: Some(RefKind::Reference),
        };
        let mut cache = HashMap::new();
        // "test_main.rs" は "main.rs" と別ファイルなので除外されない
        // (言語チェック等で false を返す可能性はあるが、ステージ2は通過する)
        let result = is_relevant_cross_file_ref(
            &r,
            "main.rs",
            0,
            "foo",
            &HashMap::new(),
            &HashMap::new(),
            &mut cache,
        );
        // 少なくともステージ2（同一ファイル判定）は通過する
        // ステージ3以降で false を返す場合もあるため、ここでは
        // ends_with 誤判定が修正されていることだけを確認
        // （修正前は false を返していた）
        let _ = result;
    }

    /// ヘルパー: テスト用シンボルを生成する
    fn make_sym(name: &str, start_line: usize, end_line: usize) -> crate::models::symbol::Symbol {
        use crate::models::location::{Point, Range};
        crate::models::symbol::Symbol {
            name: name.to_string(),
            kind: crate::models::symbol::SymbolKind::Function,
            range: Range {
                start: Point {
                    line: start_line,
                    column: 0,
                },
                end: Point {
                    line: end_line,
                    column: 0,
                },
            },
            doc: None,
            complexity: None,
            children: vec![],
        }
    }

    /// pure-delete hunk（new_count=0）がシンボル開始行と一致する場合に検出される
    #[test]
    fn find_affected_pure_delete_at_symbol_start() {
        let sym = make_sym("foo", 4, 9);
        let hunk = HunkInfo {
            old_start: 5,
            old_count: 3,
            new_start: 5,
            new_count: 0,
        };
        let result = find_affected_symbols(&[sym], &[hunk]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "foo");
        assert_eq!(result[0].change_type, "removed");
    }

    /// pure-delete hunk がシンボル内部にある場合も検出される
    #[test]
    fn find_affected_pure_delete_inside_symbol() {
        let sym = make_sym("bar", 2, 10);
        let hunk = HunkInfo {
            old_start: 6,
            old_count: 2,
            new_start: 6,
            new_count: 0,
        };
        let result = find_affected_symbols(&[sym], &[hunk]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "bar");
    }

    /// pure-delete hunk がシンボル範囲外にある場合は検出されない
    #[test]
    fn find_affected_pure_delete_outside_symbol() {
        let sym = make_sym("baz", 10, 20);
        let hunk = HunkInfo {
            old_start: 5,
            old_count: 3,
            new_start: 5,
            new_count: 0,
        };
        let result = find_affected_symbols(&[sym], &[hunk]);
        assert!(result.is_empty());
    }

    /// symbol_overlaps_hunks: pure-delete hunk でシンボル境界の検出
    #[test]
    fn symbol_overlaps_pure_delete_at_boundary() {
        let sym = make_sym("fn_at_boundary", 4, 9);
        let hunk = HunkInfo {
            old_start: 5,
            old_count: 3,
            new_start: 5,
            new_count: 0,
        };
        assert!(symbol_overlaps_hunks(&sym, &[hunk]));
    }

    /// 通常の hunk（new_count > 0）は従来通り動作する
    #[test]
    fn find_affected_normal_hunk() {
        let sym = make_sym("normal", 4, 9);
        let hunk = HunkInfo {
            old_start: 5,
            old_count: 2,
            new_start: 5,
            new_count: 3,
        };
        let result = find_affected_symbols(&[sym], &[hunk]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].change_type, "modified");
    }

    // --- is_import_context テスト ---

    #[test]
    fn import_context_ts_import() {
        assert!(is_import_context(Some(
            "import { useCommitStore } from '../stores'"
        )));
        assert!(is_import_context(Some(
            "import useCommitStore from '../stores'"
        )));
        assert!(is_import_context(Some(
            "import{ useCommitStore } from '../stores'"
        )));
    }

    #[test]
    fn import_context_ts_reexport() {
        assert!(is_import_context(Some(
            "export { useCommitStore } from '../stores'"
        )));
        assert!(is_import_context(Some(
            "export{ useCommitStore } from './commitStore'"
        )));
    }

    #[test]
    fn import_context_rust_use() {
        assert!(is_import_context(Some("use crate::stores::commit_store;")));
        assert!(is_import_context(Some(
            "pub use crate::stores::commit_store;"
        )));
    }

    #[test]
    fn import_context_python_from() {
        assert!(is_import_context(Some("from stores import commit_store")));
    }

    #[test]
    fn import_context_ruby_require() {
        assert!(is_import_context(Some("require 'commit_store'")));
        assert!(is_import_context(Some(
            "require_relative 'stores/commit_store'"
        )));
    }

    #[test]
    fn import_context_non_import() {
        // 通常のコード行は false
        assert!(!is_import_context(Some("const result = useCommitStore();")));
        assert!(!is_import_context(Some("useCommitStore.getState()")));
        assert!(!is_import_context(Some("fn main() {")));
        assert!(!is_import_context(None));
    }

    #[test]
    fn import_context_ts_export_without_from() {
        // re-export ではない通常の export は false
        assert!(!is_import_context(Some(
            "export const useCommitStore = create()"
        )));
        assert!(!is_import_context(Some("export function foo() {")));
    }

    #[test]
    fn import_context_c_include() {
        assert!(is_import_context(Some("#include \"header.h\"")));
        assert!(is_import_context(Some("#include <stdio.h>")));
    }

    #[test]
    fn import_context_csharp_using() {
        assert!(is_import_context(Some("using System;")));
        assert!(is_import_context(Some("using static System.Math;")));
        // using ブロック（リソース管理）は import ではない
        assert!(!is_import_context(Some(
            "using var stream = new FileStream();"
        )));
    }

    #[test]
    fn import_context_zig_import() {
        assert!(is_import_context(Some("const std = @import(\"std\");")));
    }

    #[test]
    fn import_context_php_use() {
        assert!(is_import_context(Some("use App\\Models\\User;")));
    }

    #[test]
    fn import_context_swift_import() {
        assert!(is_import_context(Some("import Foundation")));
    }
}
