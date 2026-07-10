mod collector;
mod filters;
mod import_facts;
mod pass2;
mod pass3;
mod signature;
pub(crate) mod test_context;
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
    /// この FileContext に対して cross-file 参照検索結果を流し込んでよい
    /// シンボル名（`ci_key` 正規化済み）の集合。
    ///
    /// `should_include_for_cross_file` を **必ず per-file で実行**して詰める。
    /// グローバルな `included_symbols` は Aho-Corasick 用の union だけに使い、
    /// pass2 の `sym_to_fc` 構築は本フィールドだけを見ること。
    ///
    /// 同名シンボル (例: 異なる class の `new`) が複数ファイルに存在する場合、
    /// 「modified の Factory.php は include / added の Id.php は exclude」の
    /// ように **per-file 単位で** 振り分けられる。
    cross_file_symbol_keys: HashSet<String>,
    /// `affected` の `ci_key(lang_id, name)` → 元の name の事前 index。
    /// on_ref ホットループでの per-ref 線形 `find` + `ci_key` の String 割当を O(1) 参照に
    /// 置換する (旧実装は ref ごとに `affected.iter().find(|a| ci_key(..)==key)` を実行していた)。
    /// 同一 ci_key が複数あれば先勝ち (旧 `.find()` の最初一致と挙動一致)。
    affected_name_by_cikey: HashMap<String, String>,
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

/// unified diff のワークスペースディレクトリ内での影響を解析する streaming API。
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
///
/// `FileImpact` を 1 件生成するごとに `on_file_impact` callback に渡し、`Vec<FileImpact>`
/// を全件 memory に貯めないため、呼び出し側（CLI）で JSON を 1 件ずつ stdout に flush
/// すれば、最終 `ContextResult.changes` の成長に伴う数 GB 級のピーク RSS を排除できる。
///
/// `options.exclude_dirs` / `options.exclude_globs` は Pass2 cross-file 検索から
/// 追加で除外したい対象を指定する (固定の `IMPACT_DEFAULT_EXCLUDED_DIRS` にマージして適用)。
pub fn analyze_impact_streaming<F>(
    diff_input: &str,
    dir: &Path,
    options: &crate::models::impact::ContextAnalysisOptions,
    mut on_file_impact: F,
) -> Result<()>
where
    F: FnMut(FileImpact) -> Result<()>,
{
    use crate::commands::log_phase;
    let diff_files = diff::parse_unified_diff(diff_input);
    // 全 changed file が lexer-only 言語 (Xojo) の場合は cross-file impact 解析を skip。
    // 理由: lexer 経路の cross-file refs は汎用名 noise が多く実用精度が出ない (本格対応は将来 PR)。
    // env 名は後方互換のため v26.5 系 `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT` を維持。
    let force_ci = std::env::var("ASTRO_SIGHT_FORCE_CI_LANG_IMPACT")
        .ok()
        .as_deref()
        == Some("1");
    if diff_files_all_case_insensitive(&diff_files) && !force_ci {
        log_phase("context.skip_ci_only", "applied", 0);
        return Ok(());
    }

    let t = std::time::Instant::now();
    log_phase("context.pass1", "start", 0);
    let (file_contexts, all_symbol_names, method_parent_types, included_symbols) =
        collect_affected_symbols(diff_input, &diff_files, dir);
    log_phase("context.pass1", "end", t.elapsed().as_millis());
    log_phase(
        &format!(
            "context.pass1.stats files={} all_syms={} included={}",
            file_contexts.len(),
            all_symbol_names.len(),
            included_symbols.len()
        ),
        "info",
        0,
    );

    if all_symbol_names.is_empty() {
        let t = std::time::Instant::now();
        log_phase("context.assemble_no_cross", "start", 0);
        for change in assemble_without_cross_file(file_contexts, &included_symbols) {
            // streaming 経路 (pass34) と同じ空 FileImpact スキップを適用する。
            // 片側にしか無いと「全ファイルが候補ゼロ」の diff でだけ空エントリが出力され、
            // 同一変更の出力有無が同居する他ファイルの内容に依存してしまう。
            if change.affected_symbols.is_empty()
                && change.impacted_callers.is_empty()
                && change.signature_changes.is_empty()
                && change.low_confidence_callers.is_empty()
                && change.informational_callers.is_empty()
            {
                continue;
            }
            on_file_impact(change)?;
        }
        log_phase("context.assemble_no_cross", "end", t.elapsed().as_millis());
        return Ok(());
    }

    let mut sym_ix: HashMap<String, usize> = HashMap::with_capacity(all_symbol_names.len());
    for (ix, name) in all_symbol_names.iter().enumerate() {
        sym_ix.insert(name.clone(), ix);
    }

    // Pass 2: per-file で Definition 集合と References を同時収集し、caller_maps に即流す。
    // Phase 4: 低確信度 caller (BareNameOnly + generic name) は別バケット
    // (`typed_low_caller_maps`) へ振り分けて強い impact 信号を汚染しない。
    let t = std::time::Instant::now();
    log_phase("context.pass2", "start", 0);
    let pass2_maps = stream_caller_maps_and_defs(
        &file_contexts,
        &all_symbol_names,
        &sym_ix,
        &method_parent_types,
        dir,
        options,
    );
    log_phase("context.pass2", "end", t.elapsed().as_millis());
    let crate::engine::impact::pass2::StreamCallerMaps {
        mut caller_maps,
        mut low_caller_maps,
        mut informational_caller_maps,
        def_paths_by_ix,
        string_pool,
    } = pass2_maps;

    // Stage 4b 判定用: method parent を持つ sym_ix のビットセット
    let has_parent_by_ix = compute_has_parent_by_ix(&sym_ix, &method_parent_types);

    // Pass 3/4 融合: 各 FileContext を 1 件ずつ取り出し、de-intern → FileImpact → callback → drop。
    // 旧実装は `Vec<CallerMap>` 全件を String 化してから `FileImpact` を作っていたため、
    // 中間表現が 2 重に materialize されて RSS の 0.7-1.2 GB を食っていた（codex 分析）。
    // さらに streaming callback で呼び出し側（CLI）へ即渡し、`Vec<FileImpact>` の累積も廃止する。
    let t = std::time::Instant::now();
    log_phase("context.pass34", "start", 0);
    for (fc_ix, ctx) in file_contexts.into_iter().enumerate() {
        let typed_map = std::mem::take(&mut caller_maps[fc_ix]);
        let typed_low_map = std::mem::take(&mut low_caller_maps[fc_ix]);
        let typed_informational_map = std::mem::take(&mut informational_caller_maps[fc_ix]);
        let caller_map = apply_stage4b_single(
            typed_map,
            &def_paths_by_ix,
            &string_pool,
            &has_parent_by_ix,
            &ctx.new_path,
        );
        let low_caller_map = apply_stage4b_single(
            typed_low_map,
            &def_paths_by_ix,
            &string_pool,
            &has_parent_by_ix,
            &ctx.new_path,
        );
        let informational_caller_map = apply_stage4b_single(
            typed_informational_map,
            &def_paths_by_ix,
            &string_pool,
            &has_parent_by_ix,
            &ctx.new_path,
        );
        let impact = build_file_impact(ctx, caller_map, low_caller_map, informational_caller_map);
        // affected_symbols / impacted_callers / signature_changes / low_confidence_callers /
        // informational_callers が
        // すべて空の FileImpact は解析対象外（AST が抽出できなかった minified / dist / 生成物
        // ファイル等）なので出力せずスキップする。大規模リポジトリでは dist/*.js 等で数千件の
        // 空 FileImpact が発生し、stdout への書き出しだけで数 GB に達するのを防ぐ。
        if impact.affected_symbols.is_empty()
            && impact.impacted_callers.is_empty()
            && impact.signature_changes.is_empty()
            && impact.low_confidence_callers.is_empty()
            && impact.informational_callers.is_empty()
        {
            continue;
        }
        on_file_impact(impact)?;
        // caller_map / typed_map は scope 終了で drop、FileImpact は callback に consume される。
    }
    drop(caller_maps);
    drop(low_caller_maps);
    drop(informational_caller_maps);
    drop(string_pool);
    log_phase("context.pass34", "end", t.elapsed().as_millis());

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
            low_confidence_callers: Vec::new(),
            informational_callers: Vec::new(),
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

    use crate::commands::log_phase;
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

        log_phase(
            &format!("context.pass1.file path={}", df.new_path),
            "start",
            0,
        );

        let t = std::time::Instant::now();
        let utf8_path = Utf8Path::new(file_path.to_str().unwrap_or(""));
        let source = match parser::read_file(utf8_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        log_phase("context.pass1.read_file", "end", t.elapsed().as_millis());

        let t = std::time::Instant::now();
        let (tree, lang_id) = match parser::parse_file(utf8_path, &source) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let root = tree.root_node();
        log_phase("context.pass1.parse", "end", t.elapsed().as_millis());

        let t = std::time::Instant::now();
        let syms = symbols::extract_symbols(root, &source, lang_id).unwrap_or_default();
        log_phase(
            &format!("context.pass1.extract_symbols n={}", syms.len()),
            "end",
            t.elapsed().as_millis(),
        );

        let t = std::time::Instant::now();
        // diff の `+` 行 (実変更行) を抽出して hunk context-only overlap を除外する。
        // 隣接 hunk の context 3 行に巻き込まれた本体未変更 export
        // (Issue 2026-05-14-private-const-and-unchanged-export-noise) を排除する。
        let changed_new_lines = diff::extract_changed_new_lines(diff_input, &df.new_path);
        let affected_raw = find_affected_symbols(&syms, &df.hunks, Some(&changed_new_lines));
        log_phase(
            &format!("context.pass1.find_affected n={}", affected_raw.len()),
            "end",
            t.elapsed().as_millis(),
        );

        // テストシンボルとローカルスコープ変数を affected から除外。
        // ローカル変数（関数内 const/let 等）はファイル外への影響を持たないため、
        // affected_symbols 出力と cross-file 伝播の両方からノイズを除去する。
        let t = std::time::Instant::now();
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
        log_phase(
            &format!("context.pass1.filter n={}", affected.len()),
            "end",
            t.elapsed().as_millis(),
        );

        let t = std::time::Instant::now();
        let sig_changes = detect_signature_changes(diff_input, &df.new_path, &affected, lang_id);
        log_phase(
            &format!("context.pass1.detect_sig n={}", sig_changes.len()),
            "end",
            t.elapsed().as_millis(),
        );

        let t = std::time::Instant::now();
        let call_edges = calls::extract_calls(root, &source, lang_id, None).unwrap_or_default();
        log_phase(
            &format!("context.pass1.extract_calls n={}", call_edges.len()),
            "end",
            t.elapsed().as_millis(),
        );

        // per-file の cross-file ルーティング対象集合。
        // 同名シンボルでも他の FileContext と独立に判定する必要があるため、
        // `symbol_name_set.contains` で early-skip してはいけない（codex 分析）。
        let mut cross_file_symbol_keys: HashSet<String> = HashSet::new();
        for sym in &affected {
            let sym_key = ci_key(lang_id, &sym.name);
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
            // この FileContext にとっての cross-file 検索対象に追加。
            // すでに別ファイル経由でグローバル set に登録済みでも、本ファイル
            // 固有のルーティング判定は失われない。
            cross_file_symbol_keys.insert(sym_key.clone());
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

        let mut affected_name_by_cikey: HashMap<String, String> = HashMap::new();
        for a in &affected {
            affected_name_by_cikey
                .entry(ci_key(lang_id, &a.name))
                .or_insert_with(|| a.name.clone());
        }

        file_contexts.push(FileContext {
            new_path: df.new_path.clone(),
            lang_id,
            affected,
            sig_changes,
            hunks,
            call_edges,
            cross_file_symbol_keys,
            affected_name_by_cikey,
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
    // hunks と重なる定義シンボルは (syms, sym.name, hunks) が不変なので 1 回だけ引く。
    // Option<&Symbol> は Copy のため以降の各判定で使い回せる (旧実装は 3 回線形スキャンしていた)。
    let overlapping = find_overlapping_symbol(syms, &sym.name, hunks);
    // 2. テストコンテキスト内のシンボルをスキップ
    if overlapping.is_some_and(|s| is_in_test_context(root, source, &s.range, lang_id, file_path)) {
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
        && overlapping.is_some_and(|s| symbols::is_override_method(root, source, lang_id, &s.range))
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
    if !overlapping.is_some_and(|s| symbols::is_symbol_exported(root, source, lang_id, &s.range)) {
        return false;
    }
    // 5. 変更行にシンボル名が出現しない場合スキップ
    if !is_symbol_in_changed_lines(diff_input, file_path, &sym.name, lang_id) {
        return false;
    }
    // 6. 新規追加シンボル (change_type == "added") は cross-file caller がまだない
    //    ため検索対象から除外する。同一 commit 内で追加された caller は他ファイルの
    //    シンボル変更経由で別途検出されるため、本シンボル単独の cross-file 探索は
    //    ノイズだけが残る。新規ファイルの全シンボルがここで除外される。
    if sym.change_type == "added" {
        return false;
    }
    true
}

/// シンボルの全行 (`sym_start..=sym_end`, 0-indexed) が新規追加行 (`changed_new_lines`)
/// に含まれるか。全行が `+` 行なら、そのシンボルは純粋な新規追加 (既存行の書き換えでない)。
fn symbol_lines_all_added(
    sym_start: usize,
    sym_end: usize,
    changed_new_lines: &std::collections::HashSet<usize>,
) -> bool {
    (sym_start..=sym_end).all(|l| changed_new_lines.contains(&l))
}

/// hunk に削除行 (`-` 行) が含まれるかを old/new の行数整合から算術的に導出する。
/// 純追加 hunk では `new_count == old_count(=context 行数) + added_in_hunk` が成立する。
/// 削除があると old_count に new 側へ現れない行が含まれ、この等式が崩れる。これにより
/// 削除本文を持たない `HunkInfo` でも (old 側 parse 不要で) 削除有無を判定できる。
fn hunk_has_removed_lines(
    hunk: &HunkInfo,
    hunk_start: usize,
    hunk_end: usize,
    changed_new_lines: &std::collections::HashSet<usize>,
) -> bool {
    let added_in_hunk = (hunk_start..hunk_end)
        .filter(|l| changed_new_lines.contains(l))
        .count();
    // old_count は外部 diff 入力 (context --diff / --diff-file / session) では巨大値に
    // なり得るため saturating_add で overflow を防ぐ。wrap すると削除を見逃して "added" に
    // 倒れる fail-open になるが、saturate すれば usize::MAX != new_count = 削除あり扱いの
    // "modified" に倒れて fail-closed になる。
    hunk.old_count.saturating_add(added_in_hunk) != hunk.new_count
}

/// hunk をシンボル範囲と照合して affected シンボルを検出する。
///
/// `changed_new_lines` が `Some` のときは、`+` 行が 1 つも symbol range に入らない
/// (= context 行だけで overlap した) symbol を除外する。pure-delete hunk
/// (new_count==0) は `+` 行が無いため、この追加フィルタを適用せず従来通り
/// change_type=removed として残す。
fn find_affected_symbols(
    syms: &[crate::models::symbol::Symbol],
    hunks: &[HunkInfo],
    changed_new_lines: Option<&std::collections::HashSet<usize>>,
) -> Vec<AffectedSymbol> {
    let mut affected = Vec::new();

    for sym in syms {
        for hunk in hunks {
            let hunk_start = hunk.new_start.saturating_sub(1); // 1-indexed to 0-indexed
            let hunk_end = hunk_start.saturating_add(hunk.new_count);
            let sym_start = sym.range.start.line;
            let sym_end = sym.range.end.line;

            // オーバーラップチェック。
            // tree-sitter の range.end.line は包含的（シンボル最終バイトが乗る行）
            // なので、通常 hunk では sym_end を含めて判定する。これにより単一行
            // シンボル（start==end）や複数行シンボルの最終行のみの変更も検出する。
            // ゼロ幅 hunk（pure-delete）は新ファイル側に行が無く、境界一致は
            // 隣接行の削除を指すため、従来どおり半開区間（< sym_end）で判定する。
            let overlaps = if hunk.new_count == 0 {
                hunk_start >= sym_start && hunk_start < sym_end
            } else {
                hunk_start <= sym_end && hunk_end > sym_start
            };
            if overlaps {
                // context-only overlap の除外: changed_new_lines が Some かつ
                // pure-delete でない hunk について、symbol range 内に `+` 行が
                // 存在しなければ context だけの overlap と判断して skip する。
                // pure-delete (new_count==0) は `+` 行が存在しないため、この
                // フィルタを適用すると change_type=removed が出なくなる。
                if let Some(cl) = changed_new_lines
                    && hunk.new_count > 0
                    && !(sym_start..=sym_end).any(|l| cl.contains(&l))
                {
                    continue;
                }
                // change_type 判定:
                // - new_count==0: pure delete → "removed"。
                // - シンボル全行が新規追加行 (changed_new_lines に全行含まれる) かつ
                //   その hunk に削除行が無い → "added"。既存ファイルの context 込み hunk
                //   (old_count>0) でも、純追加で挿入された新規シンボルを正しく "added" と
                //   判定する (Issue: 2026-06-14-antigravity-new-symbol-impact)。削除行の
                //   有無は old/new の行数整合から算術的に導出する (old 側 parse 不要)。
                //   近接削除を含む混在 hunk や複数 hunk にまたがるシンボルは all_added が
                //   崩れるため "modified" に倒れる (fail-closed、false negative を避ける)。
                // - hunk old_count==0: シンボル全体を hunk が覆う場合のみ "added"。hunk が
                //   部分的にしか覆わない場合は既存シンボル内への行追加なので "modified"。
                // - それ以外: "modified"。
                let all_added_no_removal = changed_new_lines.is_some_and(|cl| {
                    symbol_lines_all_added(sym_start, sym_end, cl)
                        && !hunk_has_removed_lines(hunk, hunk_start, hunk_end, cl)
                });
                let change_type = if hunk.new_count == 0 {
                    "removed"
                } else if all_added_no_removal {
                    "added"
                } else if hunk.old_count == 0 {
                    // hunk が sym 全体（包含的な最終行 sym_end を含む）を覆う場合のみ
                    // "added"。hunk_end は排他的上限なので sym_end を含むには
                    // hunk_end > sym_end が必要。部分的にしか覆わない場合は既存
                    // シンボル内への追加なので "modified"（cross-file 探索の対象に残す）。
                    let hunk_covers_symbol = hunk_start <= sym_start && hunk_end > sym_end;
                    if hunk_covers_symbol {
                        "added"
                    } else {
                        "modified"
                    }
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
        let hunk_end = hunk_start.saturating_add(h.new_count);
        // range.end.line は包含的なので通常 hunk は end を含めて判定する
        // （単一行シンボル・最終行のみの変更を取りこぼさない）。
        // ゼロ幅 hunk（pure-delete）は従来どおり半開区間で判定する。
        if h.new_count == 0 {
            hunk_start >= sym.range.start.line && hunk_start < sym.range.end.line
        } else {
            hunk_start <= sym.range.end.line && hunk_end > sym.range.start.line
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

/// impl/class/trait/interface/enum ブロック内のメソッドの親型名を取得する。
///
/// Rust `impl Foo { fn bar() {} }` → `Some("Foo")` を返す
/// Rust `impl Trait for Foo { fn bar() {} }` → `Some("Foo")` を返す
/// Rust `trait Foo { fn bar() {} }` → `Some("Foo")` を返す
/// クラスベース言語 → クラス/トレイト/インタフェース/enum 名を返す
/// Zig `const Foo = struct { fn bar() {} };` → `Some("Foo")` を返す
///
/// **PHP trait の認識は load-bearing**: 大規模 Laravel/DDD 系プロジェクトで
/// `trait Factory` のような trait 内 method 変更時、`find_parent_type_name` が
/// None を返すと `method_parent_types` から該当エントリが消え Stage 4b の
/// parent_in_this_file チェックが完全にバイパスされる。結果として
/// `Other::new()` 等の同名 method 全件が誤って impacted_callers に流れる
/// (実測: 1 リポで数千件規模の偽陽性)。
///
/// **namespace は意図的に対象外**: TS `internal_module` / C++ `namespace_definition`
/// は「scope」であって「親型」ではない。Stage 4b に混ぜると非修飾呼び出しや
/// alias import を持つ正当な参照を落としてしまうため対象外とする。
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
        // Zig は `const Foo = struct { ... };` の形なので親型名は宣言ノード自身ではなく、
        // 1つ上の variable_declaration の identifier から取る。
        if lang_id == LangId::Zig
            && matches!(
                n.kind(),
                "struct_declaration" | "enum_declaration" | "union_declaration"
            )
            && let Some(name) = zig_container_binding_name(n, source)
        {
            return Some(name);
        }
        // クラス/トレイト/インタフェース/enum/protocol/record/struct 系の宣言ノード。
        // tree-sitter-php: class_declaration / trait_declaration / interface_declaration / enum_declaration
        // tree-sitter-{java,kotlin,c-sharp,typescript}: class_declaration / interface_declaration / enum_declaration
        // tree-sitter-typescript: abstract_class_declaration (TS abstract class)
        // tree-sitter-c-sharp: struct_declaration / record_declaration (C# 9+)
        // tree-sitter-cpp: class_specifier / struct_specifier / enum_specifier (C++11 enum class 含む)
        // tree-sitter-{python,ruby}: class_definition / class
        // tree-sitter-{ruby}: module / singleton_class
        // tree-sitter-rust: trait_item (trait 内メソッドの親)
        // tree-sitter-swift: protocol_declaration (protocol 内宣言の親)
        // tree-sitter-{go}: type_declaration を struct/interface 含めて拾うのは困難なため除外 (Go は別経路)
        if matches!(
            n.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "class_definition"
                | "class_specifier"
                | "struct_specifier"
                | "struct_declaration"
                | "record_declaration"
                | "trait_declaration"
                | "trait_item"
                | "interface_declaration"
                | "protocol_declaration"
                | "enum_declaration"
                | "enum_specifier"
                | "module"
                | "module_declaration"
                | "singleton_class"
                | "class"
                | "object_declaration"
        ) {
            if let Some(name) = n
                .child_by_field_name("name")
                .and_then(|name| extract_type_name(name, source))
            {
                return Some(name);
            }
            // Ruby `class Foo` / `module Foo` は name フィールドではなく
            // 子ノードの constant / scope_resolution として配置される。
            let count = n.named_child_count() as u32;
            for i in 0..count {
                if let Some(child) = n.named_child(i)
                    && matches!(child.kind(), "constant" | "scope_resolution" | "identifier")
                    && let Ok(text) = child.utf8_text(source)
                {
                    return Some(text.to_string());
                }
            }
        }
        current = n.parent();
    }
    None
}

/// Zig の `const Foo = struct { ... };` 形式で、struct/enum/union 宣言ノードの
/// 親 variable_declaration から束縛名を取得する。
fn zig_container_binding_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "variable_declaration" {
        return None;
    }
    let count = parent.named_child_count() as u32;
    for i in 0..count {
        if let Some(child) = parent.named_child(i)
            && child.kind() == "identifier"
            && let Ok(text) = child.utf8_text(source)
        {
            return Some(text.to_string());
        }
    }
    None
}

/// tree-sitter の型ノードから型名を抽出する（ジェネリクスやスコープ付き型を処理）。
///
/// Swift の `simple_identifier` / C++ の `qualified_identifier` も
/// 親型ノードの name field として現れるので拾えるようにしてある。
fn extract_type_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" | "simple_identifier" => {
            node.utf8_text(source).ok().map(|s| s.to_string())
        }
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|t| extract_type_name(t, source)),
        "scoped_type_identifier" | "qualified_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| extract_type_name(n, source)),
        _ => node.utf8_text(source).ok().map(|s| s.to_string()),
    }
}

/// diff パスが安全か検証する（絶対パスやトラバーサルコンポーネントを拒否）。
pub(crate) fn is_safe_diff_path(path: &str) -> bool {
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

/// diff 内で実際に変更されたファイルパスを言語判定用に取り出す。
/// 削除 diff は `new_path` が `/dev/null` になるため、旧パスで判定する。
fn diff_file_path_for_language(df: &DiffFile) -> &str {
    if df.new_path == "/dev/null" {
        &df.old_path
    } else {
        &df.new_path
    }
}

/// diff の解析対象が case-insensitive 言語だけかどうかを判定する。
///
/// Xojo などの case-insensitive GLR 系 grammar は小さな diff でも parse 時に
/// メモリが線形膨張するため、対象がそれらの言語だけなら Pass1 すら起動しない。
/// `/dev/null` を含む削除 diff では旧パスを使い、削除だけの Xojo 変更も確実に skip する。
pub(crate) fn diff_files_all_case_insensitive(diff_files: &[DiffFile]) -> bool {
    let mut seen_analyzable_file = false;

    for df in diff_files {
        let path = diff_file_path_for_language(df);
        if path == "/dev/null" || !is_safe_diff_path(path) {
            continue;
        }

        let Ok(lang) = LangId::from_path(Utf8Path::new(path)) else {
            return false;
        };
        seen_analyzable_file = true;
        if !lang.is_case_insensitive() {
            return false;
        }
    }

    seen_analyzable_file
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

    fn diff_file(old_path: &str, new_path: &str) -> DiffFile {
        DiffFile {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        }
    }

    // Xojo などの CI 言語だけの diff は Pass1 より前にスキップ対象になる
    #[test]
    fn diff_files_all_case_insensitive_accepts_xojo_change() {
        let files = vec![diff_file("sample.xojo_code", "sample.xojo_code")];
        assert!(diff_files_all_case_insensitive(&files));
    }

    // 削除 diff は new_path が /dev/null になるため旧パスで CI 言語判定する
    #[test]
    fn diff_files_all_case_insensitive_uses_old_path_for_deletion() {
        let files = vec![diff_file("sample.xojo_code", "/dev/null")];
        assert!(diff_files_all_case_insensitive(&files));
    }

    // 非 CI 言語が混ざる diff は通常解析に回す
    #[test]
    fn diff_files_all_case_insensitive_rejects_mixed_languages() {
        let files = vec![
            diff_file("sample.xojo_code", "sample.xojo_code"),
            diff_file("src/lib.rs", "src/lib.rs"),
        ];
        assert!(!diff_files_all_case_insensitive(&files));
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
            container: None,
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
        let result = find_affected_symbols(&[sym], &[hunk], None);
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
        let result = find_affected_symbols(&[sym], &[hunk], None);
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
        let result = find_affected_symbols(&[sym], &[hunk], None);
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
        let result = find_affected_symbols(&[sym], &[hunk], None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].change_type, "modified");
    }

    /// hunk が pure add で、シンボル全体を覆う場合は "added" 判定
    #[test]
    fn find_affected_pure_add_covering_whole_symbol_is_added() {
        // シンボル: line 4-9 (6 行)
        // hunk: 新規ファイル (old_count=0)、line 1 から 20 行追加 (シンボル全域カバー)
        let sym = make_sym("new_class", 4, 9);
        let hunk = HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 20,
        };
        let result = find_affected_symbols(&[sym], &[hunk], None);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].change_type, "added",
            "hunk がシンボル全体を覆う pure add は added"
        );
    }

    /// hunk が pure add でも、既存シンボル内の部分追加は "modified" 判定
    /// (既存関数本体に新規行を 1 行追加した場合などが該当する)
    #[test]
    fn find_affected_pure_add_inside_existing_symbol_is_modified() {
        // シンボル: line 4-9 (6 行) — 既存関数
        // hunk: line 6 から 1 行だけ pure add (関数本体内への部分追加)
        let sym = make_sym("existing_fn", 4, 9);
        let hunk = HunkInfo {
            old_start: 6,
            old_count: 0,
            new_start: 6,
            new_count: 1,
        };
        let result = find_affected_symbols(&[sym], &[hunk], None);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].change_type, "modified",
            "既存シンボル内への部分追加 (hunk が symbol を覆わない) は modified"
        );
    }

    #[test]
    fn huge_hunk_range_does_not_overflow() {
        let sym = make_sym("normal", 4, 9);
        let hunk = HunkInfo {
            old_start: usize::MAX,
            old_count: 2,
            new_start: usize::MAX,
            new_count: 2,
        };

        let result = find_affected_symbols(
            std::slice::from_ref(&sym),
            std::slice::from_ref(&hunk),
            None,
        );
        assert!(result.is_empty());
        assert!(!symbol_overlaps_hunks(&sym, &[hunk]));
    }

    /// changed_new_lines が指定されている場合、context 行のみで overlap した
    /// symbol (= `+` 行が symbol range 内に 1 つも無い) は除外される
    /// (Issue 2026-05-14-private-const-and-unchanged-export-noise 修正 2)。
    #[test]
    fn find_affected_symbols_excludes_context_only_overlap() {
        // hunk 領域: line 41-78 (new_start=41, new_count=38) — 隣接 symbol を巻き込む幅
        // symbol replaceTemplate: line 42 (単一行、hunk 領域内だが + 行を含まない context)
        // changed_new_lines: 0-indexed 43-76 (実 + 行は symbol の line 42 の外)
        let sym = make_sym("replaceTemplate", 42, 42);
        let hunk = HunkInfo {
            old_start: 41,
            old_count: 3,
            new_start: 41,
            new_count: 38,
        };
        let mut changed = std::collections::HashSet::new();
        for l in 44..78usize {
            changed.insert(l - 1);
        }
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert!(
            result.is_empty(),
            "context-only overlap (+ 行が symbol range 内に無い) は除外されるべき"
        );
    }

    /// changed_new_lines が指定されていても、symbol range 内に `+` 行が 1 つでも
    /// 含まれれば従来通り affected として残る。
    #[test]
    fn find_affected_symbols_keeps_overlap_with_actual_change() {
        let sym = make_sym("modified_fn", 43, 46);
        let hunk = HunkInfo {
            old_start: 41,
            old_count: 9,
            new_start: 41,
            new_count: 10,
        };
        let mut changed = std::collections::HashSet::new();
        changed.insert(43); // 0-indexed line 43 (= 1-indexed 44) は symbol range 内
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "modified_fn");
        assert_eq!(result[0].change_type, "modified");
    }

    /// pure-delete hunk (new_count==0) は changed_new_lines が空でも従来通り
    /// change_type=removed として残る (削除アンカー判定はフィルタ対象外)。
    #[test]
    fn find_affected_symbols_pure_delete_remains_with_empty_changed_lines() {
        let sym = make_sym("removed_fn", 4, 9);
        let hunk = HunkInfo {
            old_start: 5,
            old_count: 3,
            new_start: 5,
            new_count: 0,
        };
        let changed = std::collections::HashSet::new();
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].change_type, "removed");
    }

    /// 単一行シンボル（range.start.line == range.end.line）の変更が検出される。
    /// tree-sitter の range.end.line は包含的なので、単一行の const/type/1 行関数の
    /// API 破壊が impact/review から脱落してはならない（回帰: 2026-05-31）。
    #[test]
    fn find_affected_detects_single_line_symbol() {
        // 単一行シンボル: line 0 (start == end == 0)
        let sym = make_sym("MAX_RETRIES", 0, 0);
        let hunk = HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        };
        let mut changed = std::collections::HashSet::new();
        changed.insert(0); // 0-indexed line 0 が実変更行
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1, "単一行シンボルの変更が検出されるべき");
        assert_eq!(result[0].name, "MAX_RETRIES");
        assert_eq!(result[0].change_type, "modified");
        // symbol_overlaps_hunks でも同様に単一行の境界を検出する
        let sym2 = make_sym("MAX_RETRIES", 0, 0);
        let hunk2 = HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        };
        assert!(symbol_overlaps_hunks(&sym2, &[hunk2]));
    }

    /// pure-add (old_count==0) が sym 先頭から始まっても、最終行（包含的な sym_end）を
    /// 覆わなければ "added" ではなく "modified"。range.end.line は包含的なので full-cover
    /// 判定には hunk_end > sym_end が必要で、off-by-one で "added" 誤判定すると cross-file
    /// 探索から脱落して false negative になる（回帰: 2026-05-31）。
    #[test]
    fn find_affected_pure_add_not_covering_last_line_is_modified() {
        let sym = make_sym("partial", 4, 9); // 6 行 (4..=9)
        let hunk = HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 5, // hunk_start = 4 (sym_start と一致)
            new_count: 5, // hunk_end = 9 → 包含的な sym_end(9) を覆わない
        };
        let result = find_affected_symbols(&[sym], &[hunk], None);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].change_type, "modified",
            "最終行を覆わない pure-add は added ではなく modified"
        );
    }

    /// 既存ファイルの末尾/途中に新規シンボルを追加すると git diff が context 行を
    /// hunk に含めて old_count>0 になるが、シンボル全行が `+` 行 (削除なし) なら
    /// "added" と判定する (Issue: 2026-06-14-antigravity-new-symbol-impact)。
    /// これにより hook の `change_type != "added"` 除外で誤ブロッキングが消える。
    #[test]
    fn find_affected_context_hunk_new_symbol_all_added_no_removals_is_added() {
        // 新ファイル: 行1-3 既存 AppCfg (context), 行4 空行(+), 行5-8 新規 struct(+)
        // hunk: @@ -1,3 +1,8 @@ (old_count=3 の context 込み hunk)
        let sym = make_sym("AntigravityCfg", 4, 7); // 0-indexed 4..=7 (struct 4 行)
        let hunk = HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 8,
        };
        // changed_new_lines: 0-indexed 3..=7 (空行 + struct 全行が +)
        let mut changed = std::collections::HashSet::new();
        for l in 3..=7usize {
            changed.insert(l);
        }
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "AntigravityCfg");
        assert_eq!(
            result[0].change_type, "added",
            "context 込み hunk でも全行 + (削除なし) の新規シンボルは added"
        );
    }

    /// シンボル全行が `+` 行でも、その hunk に削除行があれば (全面書き換え) "modified"。
    /// old/new 行数整合 (old_count + added_in_hunk != new_count) で削除を検出し、
    /// 既存シンボルの書き換えを "added" と誤判定して cross-file 探索から漏らさない。
    #[test]
    fn find_affected_full_rewrite_all_new_lines_but_hunk_has_removals_is_modified() {
        // hunk: @@ -1,3 +1,4 @@ (old 3 行削除 + new 4 行追加、全行書き換え)
        // new 側 sym range 0-indexed 0..=3 は全行 + だが、hunk に削除 3 行あり
        let sym = make_sym("rewritten", 0, 3);
        let hunk = HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 4,
        };
        let mut changed = std::collections::HashSet::new();
        for l in 0..=3usize {
            changed.insert(l);
        }
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].change_type, "modified",
            "全行 + でも hunk に削除があれば書き換えなので modified (fail-closed)"
        );
    }

    /// 近接削除と新規シンボル追加が同一 hunk に混在する場合、新規シンボルが全行 + でも
    /// hunk に削除があるため "modified" に倒す (fail-closed)。削除が新規シンボルへ
    /// 影響する可能性を排除できないため保守的に扱い、false negative を避ける。
    #[test]
    fn find_affected_new_symbol_in_mixed_hunk_with_removal_is_modified_fail_closed() {
        // hunk @@ -1,3 +1,5 @@:
        //   line1 (context, new 行1)
        //  -old_line (削除)
        //  +struct NewType { (new 行2)
        //  +    field: i32,  (new 行3)
        //  +}                (new 行4)
        //   line5 (context, new 行5)
        let sym = make_sym("NewType", 1, 3); // 0-indexed 1..=3 (struct 3 行)
        let hunk = HunkInfo {
            old_start: 1,
            old_count: 3, // context2 + 削除1
            new_start: 1,
            new_count: 5, // context2 + 追加3
        };
        let mut changed = std::collections::HashSet::new();
        for l in 1..=3usize {
            changed.insert(l);
        }
        let result = find_affected_symbols(&[sym], &[hunk], Some(&changed));
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].change_type, "modified",
            "削除を含む混在 hunk の新規シンボルは modified (fail-closed)"
        );
    }
}
