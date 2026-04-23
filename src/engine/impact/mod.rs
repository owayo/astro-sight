mod collector;
mod filters;
mod pass2;
mod pass3;
mod signature;
mod test_context;
mod types;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use camino::Utf8Path;

use crate::engine::{calls, diff, parser, symbols};
use crate::language::{LangId, normalize_identifier};
use crate::models::call::CallEdge;
use crate::models::impact::{AffectedSymbol, DiffFile, FileImpact, HunkInfo, SignatureChange};
use crate::models::symbol::SymbolKind;

use pass2::stream_caller_maps_and_defs;
use pass3::{apply_stage4b_single, build_file_impact, compute_has_parent_by_ix};
use signature::{
    detect_signature_changes, is_definition_header_in_changed_lines, is_symbol_in_changed_lines,
};
use test_context::is_in_test_context;

struct FileContext {
    new_path: String,
    lang_id: LangId,
    affected: Vec<AffectedSymbol>,
    sig_changes: Vec<SignatureChange>,
    hunks: Vec<HunkInfo>,
    call_edges: Vec<CallEdge>,
}

/// キャッシュされたパース結果: (tree, ソースバッファ, 言語)。
/// `SourceBuf` を直接保持することで mmap のゼロコピー経路を維持する。
type ParsedFile = (tree_sitter::Tree, crate::engine::parser::SourceBuf, LangId);

/// `assemble_impacts` でテストコンテキスト判定に使う LRU キャッシュ上限。
/// 1 エントリあたり Tree + SourceBuf(Mmap) + LangId を保持するため、
/// 大規模リポジトリ（数万ファイル）でもピーク RSS を抑える目的で上限を設ける。
/// streaming Pass では per-file で順次走査しキャッシュ hit は同一ファイル連続時のみのため、
/// 16 でも実用上十分。worker 並列で最大 `workers × SIZE` の mmap を抱えるため小さめに保つ。
const TARGET_FILE_CACHE_SIZE: usize = 16;

/// 言語別にシンボル名を正規化した HashMap/HashSet キー。
/// 非 CI 言語ではアロケーション無し (Cow::Borrowed → into_owned は元の String 相当)、
/// CI 言語 (Xojo) では Unicode-aware に小文字化する。
fn ci_key(lang: LangId, name: &str) -> String {
    normalize_identifier(lang, name).into_owned()
}

/// unified diff のワークスペースディレクトリ内での影響を解析する。
///
/// 3 パス方式で cross-file 参照を流し込む：
///   Pass 1:  変更ファイルをパースし affected シンボルを収集
///   Pass 2:  per-file で tree-sitter parse を 1 回実行し、Definition 集合と References を
///            同時に集める。References は Stage 1-6 (Stage 4b 除く) を per-file で適用して
///            その場で `caller_map` に流し、候補 Vec を保持しない
///   Pass 3:  結合済み caller_maps に Stage 4b (competing definition) を post-filter として
///            適用し、FileImpact を組み立てる
///
/// candidate 保持を廃止し per-file で caller_map に即流すことで、worker ローカルの
/// 中間バッファを `caller_map` のサイズ (数百MB) まで抑え、融合版で発生した
/// fold 中の 1GB 級バッファ問題を排除する。
/// `FileImpact` を 1 件生成するごとに `on_file_impact` callback に渡す streaming API。
///
/// `Vec<FileImpact>` を全件 memory に貯めないため、呼び出し側（CLI）で JSON を 1 件ずつ
/// stdout に flush すれば、最終 `ContextResult.changes` の成長に伴う数 GB 級のピーク RSS を
/// 排除できる。通常の `analyze_impact` はこの API の薄い wrapper。
pub fn analyze_impact_streaming<F>(
    diff_input: &str,
    dir: &Path,
    mut on_file_impact: F,
) -> Result<()>
where
    F: FnMut(FileImpact) -> Result<()>,
{
    let diff_files = diff::parse_unified_diff(diff_input);

    let (file_contexts, all_symbol_names, method_parent_types, included_symbols) =
        collect_affected_symbols(diff_input, &diff_files, dir);

    if all_symbol_names.is_empty() {
        for change in assemble_without_cross_file(file_contexts, &included_symbols) {
            on_file_impact(change)?;
        }
        return Ok(());
    }

    let mut sym_ix: HashMap<String, usize> = HashMap::with_capacity(all_symbol_names.len());
    for (ix, name) in all_symbol_names.iter().enumerate() {
        sym_ix.insert(name.clone(), ix);
    }

    // Pass 2: per-file で Definition 集合と References を同時収集し、caller_maps に即流す
    let (mut typed_caller_maps, def_paths_by_ix, string_pool) = stream_caller_maps_and_defs(
        &file_contexts,
        &all_symbol_names,
        &sym_ix,
        &method_parent_types,
        &included_symbols,
        dir,
    );

    // Stage 4b 判定用: method parent を持つ sym_ix のビットセット
    let has_parent_by_ix = compute_has_parent_by_ix(&sym_ix, &method_parent_types);

    // Pass 3/4 融合: 各 FileContext を 1 件ずつ取り出し、de-intern → FileImpact → callback → drop。
    // 旧実装は `Vec<CallerMap>` 全件を String 化してから `FileImpact` を作っていたため、
    // 中間表現が 2 重に materialize されて RSS の 0.7-1.2 GB を食っていた（codex 分析）。
    // さらに streaming callback で呼び出し側（CLI）へ即渡し、`Vec<FileImpact>` の累積も廃止する。
    for (fc_ix, ctx) in file_contexts.into_iter().enumerate() {
        let typed_map = std::mem::take(&mut typed_caller_maps[fc_ix]);
        let caller_map = apply_stage4b_single(
            typed_map,
            &def_paths_by_ix,
            &string_pool,
            &has_parent_by_ix,
            &ctx.new_path,
        );
        let impact = build_file_impact(ctx, caller_map);
        // affected_symbols / impacted_callers / signature_changes がすべて空の FileImpact は
        // 解析対象外（AST が抽出できなかった minified / dist / 生成物ファイル等）なので出力せず
        // スキップする。大規模リポジトリでは dist/*.js 等で数千件の空 FileImpact が
        // 発生し、stdout への書き出しだけで数 GB に達するのを防ぐ。
        if impact.affected_symbols.is_empty()
            && impact.impacted_callers.is_empty()
            && impact.signature_changes.is_empty()
        {
            continue;
        }
        on_file_impact(impact)?;
        // caller_map / typed_map は scope 終了で drop、FileImpact は callback に consume される。
    }
    drop(typed_caller_maps);
    drop(string_pool);

    Ok(())
}

/// cross-file 参照が不要なケース（affected 無しなど）の軽量組み立て。
fn assemble_without_cross_file(
    file_contexts: Vec<FileContext>,
    _included_symbols: &HashSet<String>,
) -> Vec<FileImpact> {
    file_contexts
        .into_iter()
        .map(|ctx| FileImpact {
            path: ctx.new_path,
            hunks: ctx.hunks,
            affected_symbols: ctx.affected,
            signature_changes: ctx.sig_changes,
            impacted_callers: Vec::new(),
        })
        .collect()
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
        let sig_changes = detect_signature_changes(diff_input, &df.new_path, &affected, lang_id);
        let call_edges = calls::extract_calls(root, &source, lang_id, None).unwrap_or_default();

        for sym in &affected {
            let sym_key = ci_key(lang_id, &sym.name);
            if symbol_name_set.contains(&sym_key) {
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
            included_symbols.insert(sym_key.clone());
            if let Some(orig) = find_overlapping_symbol(&syms, &sym.name, &df.hunks)
                && let Some(parent_type) =
                    find_parent_type_name(root, &source, &orig.range, lang_id)
            {
                let parent_key = ci_key(lang_id, &parent_type);
                method_parent_types.insert(sym_key.clone(), parent_key.clone());
                if symbol_name_set.insert(parent_key.clone()) {
                    all_symbol_names.push(parent_key);
                }
            }
            if symbol_name_set.insert(sym_key.clone()) {
                all_symbol_names.push(sym_key);
            }
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
    // 3a. Kotlin/Java/Swift/TS/C# の `override` メソッドは親 interface/class から
    // 呼ばれるため cross-file caller を追跡できない。親 API のシグネチャは不変なので
    // 下流互換性にも影響せず、本体変更は impl 変更として扱い api.mod から除外する。
    if (sym.kind == "function" || sym.kind == "method")
        && find_overlapping_symbol(syms, &sym.name, hunks)
            .is_some_and(|s| symbols::is_override_method(root, source, lang_id, &s.range))
    {
        return false;
    }
    // 3b. 定義ヘッダが変更されていない型シンボルをスキップ。
    // 例: `trait GuestMemory` 行自体が変更されていなければ、
    // 他の変更行（フリー関数のシグネチャ等）に名前が出現しても伝播しない。
    if matches!(
        sym.kind.as_str(),
        "trait" | "struct" | "class" | "interface" | "enum"
    ) && !is_definition_header_in_changed_lines(
        diff_input, file_path, &sym.name, &sym.kind, lang_id,
    ) {
        return false;
    }
    // 4. エクスポートされていないシンボルをスキップ
    if !find_overlapping_symbol(syms, &sym.name, hunks)
        .is_some_and(|s| symbols::is_symbol_exported(root, source, lang_id, &s.range))
    {
        return false;
    }
    // 5. 変更行にシンボル名が出現しない場合スキップ
    if !is_symbol_in_changed_lines(diff_input, file_path, &sym.name, lang_id) {
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
        LangId::Xojo => 12,
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
}
