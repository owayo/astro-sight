mod signature;
mod test_context;

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::Result;
use camino::Utf8Path;
use lru::LruCache;

use crate::engine::{calls, diff, parser, refs, symbols};
use crate::language::{LangId, normalize_identifier};
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

/// キャッシュされたパース結果: (tree, ソースバッファ, 言語)。
/// `SourceBuf` を直接保持することで mmap のゼロコピー経路を維持する。
type ParsedFile = (tree_sitter::Tree, crate::engine::parser::SourceBuf, LangId);

/// `assemble_impacts` でテストコンテキスト判定に使う LRU キャッシュ上限。
/// 1 エントリあたり Tree + SourceBuf(Mmap) + LangId を保持するため、
/// 大規模リポジトリ（数万ファイル）でもピーク RSS を抑える目的で上限を設ける。
/// ほとんどの参照は同一ファイル内で連続するため少数の hot エントリで十分に hit し、
/// サイズを小さく保つことで最大メモリ量を予測可能にする。
const TARGET_FILE_CACHE_SIZE: usize = 64;

/// 言語別にシンボル名を正規化した HashMap/HashSet キー。
/// 非 CI 言語ではアロケーション無し (Cow::Borrowed → into_owned は元の String 相当)、
/// CI 言語 (Xojo) では Unicode-aware に小文字化する。
fn ci_key(lang: LangId, name: &str) -> String {
    normalize_identifier(lang, name).into_owned()
}

/// unified diff のワークスペースディレクトリ内での影響を解析する。
///
/// 4 パス方式で cross-file 参照を流し込む：
///   Pass 1:   変更ファイルをパースし affected シンボルを収集
///   Pass 1.5: 全 symbol の Definition だけを軽量収集（parent_type / competing def 判定用）
///   Pass 2+3: per-file で `Vec<SymbolReference>` を caller_map に直接集約し即 drop
///   Pass 4:   caller_maps から FileImpact を組み立てる
///
/// batch_refs 全件保持を廃止することで、数万ファイル級リポジトリでも RSS を定数に抑える。
pub fn analyze_impact(diff_input: &str, dir: &Path) -> Result<ContextResult> {
    let diff_files = diff::parse_unified_diff(diff_input);

    let (file_contexts, all_symbol_names, method_parent_types, included_symbols) =
        collect_affected_symbols(diff_input, &diff_files, dir);

    if all_symbol_names.is_empty() {
        return Ok(ContextResult {
            changes: assemble_without_cross_file(file_contexts, &included_symbols),
        });
    }

    let mut sym_ix: HashMap<String, usize> = HashMap::with_capacity(all_symbol_names.len());
    for (ix, name) in all_symbol_names.iter().enumerate() {
        sym_ix.insert(name.clone(), ix);
    }

    // Pass 1.5: 軽量 Definition path 収集
    let def_paths_by_ix =
        refs::collect_definition_paths_indexed(&all_symbol_names, dir, None).unwrap_or_default();

    // Pass 2+3: streaming に caller_maps を構築（`SymbolReference` は per-file スコープで drop）
    let caller_maps = stream_caller_maps(
        &file_contexts,
        &all_symbol_names,
        &sym_ix,
        &method_parent_types,
        &included_symbols,
        &def_paths_by_ix,
        dir,
    );

    // Pass 4: 最終組み立て
    let changes = assemble_from_caller_maps(file_contexts, caller_maps);
    Ok(ContextResult { changes })
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

/// 同一ファイル判定。サフィックスマッチで偽陽性を出さないよう、完全一致 or パス区切り付きで判定する。
fn is_same_source_file(ref_path: &str, source_path: &str) -> bool {
    ref_path == source_path || ref_path.ends_with(&format!("/{source_path}"))
}

type CallerMap = HashMap<(String, usize), (String, Vec<String>)>;

/// Pass 2+3: per-file で ref を caller_map に直接集約する streaming 実装。
///
/// 各 rayon worker はローカルな `Vec<CallerMap>` (file_contexts 数分) と `LruCache` を持ち、
/// ファイル毎に `find_refs_batch_in_file_indexed` で得た `Vec<Vec<SymbolReference>>` を
/// その場でフィルタして対応する `CallerMap` に格納する。`SymbolReference` Vec は worker 内で
/// 即 drop されるため、従来の `HashMap<sym, Vec<SymbolReference>>` の全件保持が不要になる。
///
/// Stage 4 (method parent type scoping) は「同じファイルに親型 ref があるか」を per-file の
/// `Vec<Vec<SymbolReference>>` 内で判定し、Stage 4b (competing definition) は事前に収集した
/// `def_paths_by_ix` を参照する。
#[allow(clippy::too_many_arguments)]
fn stream_caller_maps(
    file_contexts: &[FileContext],
    all_symbol_names: &[String],
    sym_ix: &HashMap<String, usize>,
    method_parent_types: &HashMap<String, String>,
    included_symbols: &HashSet<String>,
    def_paths_by_ix: &[Vec<String>],
    dir: &Path,
) -> Vec<CallerMap> {
    let n_fc = file_contexts.len();

    // sym_ix -> Vec<fc_ix>（included_symbols を満たすもののみ）
    let mut sym_to_fc: Vec<Vec<usize>> = vec![Vec::new(); all_symbol_names.len()];
    for (fc_ix, ctx) in file_contexts.iter().enumerate() {
        for sym in &ctx.affected {
            let sym_key = ci_key(ctx.lang_id, &sym.name);
            if !included_symbols.contains(&sym_key) {
                continue;
            }
            if let Some(&ix) = sym_ix.get(&sym_key) {
                sym_to_fc[ix].push(fc_ix);
            }
        }
    }

    let ac = match refs::build_ac_case_insensitive(all_symbol_names) {
        Ok(a) => a,
        Err(_) => return vec![CallerMap::new(); n_fc],
    };
    let files = match refs::collect_files(dir, None) {
        Ok(f) => f,
        Err(_) => return vec![CallerMap::new(); n_fc],
    };

    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(4);
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
    {
        Ok(p) => p,
        Err(_) => return vec![CallerMap::new(); n_fc],
    };

    type WorkerState = (Vec<CallerMap>, LruCache<String, Option<ParsedFile>>);
    let init_state = || -> WorkerState {
        (
            vec![CallerMap::new(); n_fc],
            LruCache::new(
                NonZeroUsize::new(TARGET_FILE_CACHE_SIZE).expect("cache size is non-zero"),
            ),
        )
    };

    let (maps, _cache) = pool.install(|| {
        use rayon::prelude::*;
        files
            .into_par_iter()
            .fold(init_state, |(mut local_maps, mut target_cache), path| {
                let Some(path_str) = path.to_str() else {
                    return (local_maps, target_cache);
                };
                let utf8_path = camino::Utf8Path::new(path_str);
                if let Ok(per_file) =
                    refs::find_refs_batch_in_file_indexed(all_symbol_names, &ac, utf8_path)
                {
                    accumulate_per_file(
                        &per_file,
                        &sym_to_fc,
                        file_contexts,
                        all_symbol_names,
                        sym_ix,
                        method_parent_types,
                        def_paths_by_ix,
                        &mut local_maps,
                        &mut target_cache,
                    );
                }
                (local_maps, target_cache)
            })
            .reduce(init_state, |(mut acc_maps, acc_cache), (local_maps, _)| {
                for (acc_m, local_m) in acc_maps.iter_mut().zip(local_maps.into_iter()) {
                    for (key, (name, syms)) in local_m {
                        let entry = acc_m.entry(key).or_insert_with(|| (name, Vec::new()));
                        for s in syms {
                            if !entry.1.contains(&s) {
                                entry.1.push(s);
                            }
                        }
                    }
                }
                (acc_maps, acc_cache)
            })
    });

    maps
}

/// per-file の `Vec<Vec<SymbolReference>>` を受け取り、filter 後に `CallerMap` へ流し込む。
/// `SymbolReference` Vec は本関数が終わると worker 側で drop される。
#[allow(clippy::too_many_arguments)]
fn accumulate_per_file(
    per_file: &[Vec<SymbolReference>],
    sym_to_fc: &[Vec<usize>],
    file_contexts: &[FileContext],
    all_symbol_names: &[String],
    sym_ix: &HashMap<String, usize>,
    method_parent_types: &HashMap<String, String>,
    def_paths_by_ix: &[Vec<String>],
    local_maps: &mut [CallerMap],
    target_cache: &mut LruCache<String, Option<ParsedFile>>,
) {
    for (ix, refs) in per_file.iter().enumerate() {
        let fc_ixs = &sym_to_fc[ix];
        if fc_ixs.is_empty() || refs.is_empty() {
            continue;
        }
        let sym_key = &all_symbol_names[ix];
        // 親型 ix を取得し、本ファイル内に親型 ref があるかを先に評価
        let parent_type_in_this_file = method_parent_types
            .get(sym_key)
            .and_then(|pt| sym_ix.get(pt))
            .and_then(|&pix| per_file.get(pix))
            .is_some_and(|pt_refs| !pt_refs.is_empty());
        let has_parent_type = method_parent_types.contains_key(sym_key);

        for r in refs {
            // Stage1: 定義は skip
            if r.kind == Some(RefKind::Definition) {
                continue;
            }
            for &fc_ix in fc_ixs {
                let ctx = &file_contexts[fc_ix];
                let source_path = &ctx.new_path;
                let source_lang_group = lang_compat_group(ctx.lang_id);

                // Stage2: 同一ファイル skip
                if is_same_source_file(&r.path, source_path) {
                    continue;
                }
                // Stage3: 言語互換性
                if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(&r.path))
                    && lang_compat_group(ref_lang) != source_lang_group
                {
                    continue;
                }
                // Stage4: method parent type scoping
                if has_parent_type {
                    if !parent_type_in_this_file {
                        continue;
                    }
                    // Stage4b: source_path 以外に competing definition があるか
                    if let Some(paths) = def_paths_by_ix.get(ix) {
                        let has_competing_def =
                            paths.iter().any(|p| !is_same_source_file(p, source_path));
                        if has_competing_def {
                            continue;
                        }
                    }
                }
                // Stage5: target file test context
                if is_ref_in_target_test_context(&r.path, r.line, r.column, target_cache) {
                    continue;
                }
                // Stage6: import/re-export 行
                if is_import_context(r.context.as_deref()) {
                    continue;
                }

                // 採用: CallerMap に push（SymbolReference 自体は per-file Vec に残っているが
                // 抽出した文字列のみ保持し、関数終了時に参照元 Vec は drop される）
                let affected_sym_name = ctx
                    .affected
                    .iter()
                    .find(|a| ci_key(ctx.lang_id, &a.name) == *sym_key)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| sym_key.clone());
                let caller_name = r
                    .context
                    .as_deref()
                    .and_then(extract_function_from_context)
                    .unwrap_or_else(|| affected_sym_name.clone());

                let key = (r.path.clone(), r.line);
                let entry = local_maps[fc_ix]
                    .entry(key)
                    .or_insert_with(|| (caller_name, Vec::new()));
                if !entry.1.contains(&affected_sym_name) {
                    entry.1.push(affected_sym_name);
                }
            }
        }
    }
}

/// Pass 4: caller_maps + file_contexts から最終 FileImpact を組み立てる。
/// 同一ファイル内の call_edges も統合する。
fn assemble_from_caller_maps(
    file_contexts: Vec<FileContext>,
    mut caller_maps: Vec<CallerMap>,
) -> Vec<FileImpact> {
    let mut changes = Vec::with_capacity(file_contexts.len());
    for (fc_ix, ctx) in file_contexts.into_iter().enumerate() {
        let mut caller_map = std::mem::take(&mut caller_maps[fc_ix]);

        // 同一ファイル内の call_edges をマージ
        for sym in &ctx.affected {
            let sym_key = ci_key(ctx.lang_id, &sym.name);
            for edge in &ctx.call_edges {
                if ci_key(ctx.lang_id, &edge.callee.name) == sym_key {
                    let caller_line = edge.call_site.line;
                    if !ctx.affected.iter().any(|a| {
                        ci_key(ctx.lang_id, &a.name) == ci_key(ctx.lang_id, &edge.caller.name)
                    }) {
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

    // 同一ファイル判定 (`is_same_source_file`) — 完全一致は同一扱い
    #[test]
    fn same_source_file_exact_match() {
        assert!(is_same_source_file("src/main.rs", "src/main.rs"));
    }

    // 同一ファイル判定 — パス区切り付きのサフィックスも同一扱い
    #[test]
    fn same_source_file_with_prefix() {
        assert!(is_same_source_file("other/src/main.rs", "src/main.rs"));
    }

    // 同一ファイル判定 — `test_main.rs` と `main.rs` は別ファイル（ends_with 誤判定回避）
    #[test]
    fn same_source_file_different_similar_suffix() {
        assert!(!is_same_source_file("test_main.rs", "main.rs"));
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
