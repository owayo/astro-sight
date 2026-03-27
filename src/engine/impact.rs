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
/// 5段階のフィルタを適用する：
/// 1. 定義をスキップ（呼び出し箇所の参照のみが対象）
/// 2. 同一ファイルの参照をスキップ
/// 3. 言語間の偽陽性をスキップ
/// 4. ターゲットファイルに親型が存在しない参照をスキップ（メソッド型スコーピング）
/// 5. ターゲットファイルのテストコンテキスト内の参照をスキップ
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
    if r.path.ends_with(source_path) {
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
    true
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

            // オーバーラップチェック
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
        hunk_start < sym.range.end.line && hunk_end > sym.range.start.line
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

/// diff 内の削除行(-)と追加行(+)から、affected シンボルの関数シグネチャ変更を検出する。
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
            // 次の +++ 行で処理される
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

/// 指定された関数名を含むシグネチャ行を検索する。
fn find_signature_in_lines(lines: &[String], func_name: &str) -> Option<String> {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.contains(func_name) && is_signature_line(trimmed) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// ヒューリスティック: "fn ", "def ", "function ", "func " 等を含む行をシグネチャと判定する。
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

/// コンテキスト行（例: "    symbols::extract_symbols(...)"）から関数名の抽出を試みる。
fn extract_function_from_context(context: &str) -> Option<String> {
    // "fn name" パターンを検索
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

/// 型シンボルの定義ヘッダが変更行(+/-)に出現するか確認する。
///
/// trait/struct/class/interface/enum シンボルについて、宣言キーワードに続くシンボル名
/// （例: `trait GuestMemory`, `struct Foo`）が変更行に存在するかを検査する。
/// シンボル名が他のシンボルのシグネチャ（例: `fn read_obj(m: &impl GuestMemory)`）
/// にのみ出現する場合の偽陽性を防止する。
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
        _ => return true, // 非型シンボルは常にパス
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

/// 指定ファイルの diff 変更行(+/-)にシンボル名が出現するか確認する。
///
/// 全変更行にシンボル名が存在しない場合、変更はボディのみ
/// （例: 内部の JSX/ロジック変更）であり、呼び出し元に影響しない。
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
    }
}

/// キャッシュされたパース結果: (tree, ソースバイト列, 言語)。
type ParsedFile = (tree_sitter::Tree, Vec<u8>, LangId);

/// ターゲットファイルの指定行/列の参照がテストコンテキスト内にあるか確認する。
///
/// ターゲットファイルをオンデマンドでパースし、再パースを避けるためキャッシュする。
/// `#[cfg(test)]` モジュールや `#[test]` 関数内の影響を受ける呼び出し元を除外する。
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

/// シンボルがテストコンテキスト内にあるか確認する。
///
/// テストシンボルは cross-file 影響を伝播すべきでない：
/// - テスト関数はプロダクションコードから呼ばれない
/// - テストヘルパーの変更はテストモジュールのみに影響する
///
/// 2層のアプローチを使用する：
/// 1. ファイルパスベースの判定（高速、全言語対応）
/// 2. AST ベースの判定（精密、言語固有）
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

/// ノードの前に指定テキストを含む attribute_item 兄弟が存在するか確認する。
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

    // 関数定義キーワードを含む行はシグネチャ行と判定される
    #[test]
    fn is_signature_line_detects_fn() {
        assert!(is_signature_line(
            "    pub fn process_data(x: i32) -> bool {"
        ));
        assert!(is_signature_line("def handle_request(self):"));
        assert!(is_signature_line("function calculate() {"));
    }

    // 通常のコード行はシグネチャ行と判定されない
    #[test]
    fn is_signature_line_rejects_normal_code() {
        assert!(!is_signature_line("    let x = 42;"));
        assert!(!is_signature_line("    x += 1"));
    }

    // "fn name(...)" パターンから関数名を抽出できる
    #[test]
    fn extract_function_from_context_fn() {
        assert_eq!(
            extract_function_from_context("    fn process_data(x: i32) {"),
            Some("process_data".to_string())
        );
    }

    // fn キーワードが含まれない場合は None を返す
    #[test]
    fn extract_function_from_context_no_fn() {
        assert_eq!(extract_function_from_context("let x = 42;"), None);
    }

    // Go のテストファイルパターン (*_test.go) を検出する
    #[test]
    fn is_test_file_path_go() {
        assert!(is_test_file_path("pkg/handler_test.go"));
    }

    // JS/TS の spec ファイルパターン (*.spec.ts) を検出する
    #[test]
    fn is_test_file_path_js_spec() {
        assert!(is_test_file_path("src/app.spec.ts"));
    }

    // Python のテストファイルパターン (test_*.py) を検出する
    #[test]
    fn is_test_file_path_python() {
        assert!(is_test_file_path("tests/test_main.py"));
    }

    // Ruby の spec ファイルパターン (*_spec.rb) を検出する
    #[test]
    fn is_test_file_path_ruby() {
        assert!(is_test_file_path("spec/model_spec.rb"));
    }

    // 通常のソースファイルはテストファイルと判定されない
    #[test]
    fn is_test_file_path_normal() {
        assert!(!is_test_file_path("src/main.rs"));
        assert!(!is_test_file_path("lib/handler.py"));
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

    // 変更行にシンボル名が含まれる場合 true を返す
    #[test]
    fn is_symbol_in_changed_lines_present() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-fn old_func() {}\n+fn new_func() {}";
        assert!(is_symbol_in_changed_lines(diff, "src/lib.rs", "old_func"));
        assert!(is_symbol_in_changed_lines(diff, "src/lib.rs", "new_func"));
    }

    // 変更行にシンボル名が含まれない場合 false を返す
    #[test]
    fn is_symbol_in_changed_lines_absent() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-fn old_func() {}\n+fn new_func() {}";
        assert!(!is_symbol_in_changed_lines(
            diff,
            "src/lib.rs",
            "other_func"
        ));
    }
}
