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
// `SymbolReference`/`RefKind` は旧実装で使用していたが、現在の visitor callback 化された
// per-file Pass では `refs::RefVisitor` 経由で直接 callback を受け取るため直接参照はしない。
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
pub fn analyze_impact(diff_input: &str, dir: &Path) -> Result<ContextResult> {
    // 互換 API: streaming 版で各 FileImpact を Vec に集めて返す。
    // 呼び出し側が全件 materialize を許容するケース（MCP/ライブラリ経由）で使う。
    let mut changes = Vec::new();
    analyze_impact_streaming(diff_input, dir, |impact| {
        changes.push(impact);
        Ok(())
    })?;
    Ok(ContextResult { changes })
}

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

/// method parent type を持つ sym_ix のビットセット相当の map を返す。
/// Stage 4b は method scope のシンボルだけに適用する。
fn compute_has_parent_by_ix(
    sym_ix: &HashMap<String, usize>,
    method_parent_types: &HashMap<String, String>,
) -> HashMap<u32, bool> {
    let mut has_parent: HashMap<u32, bool> = HashMap::new();
    for parent_child in method_parent_types.keys() {
        if let Some(&ix) = sym_ix.get(parent_child) {
            has_parent.insert(ix as u32, true);
        }
    }
    has_parent
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

/// 文字列の重複を取り除くための小さな interning pool。
/// caller_map のキー (path)・caller_name・sym_name は同じ文字列が大量に繰り返されるため、
/// `u32` の ID に置き換えて保持することで hashmap の key/value サイズと heap allocation
/// 数を大幅に削減する。workers=1 の streaming Pass で使うことを想定し、内部状態は
/// 単一スレッドから更新される前提（マルチ worker 利用時は `Mutex` で包んで使う）。
pub(crate) struct StringPool {
    strings: Vec<String>,
    /// `hashbrown` + `ahash` で integer-friendly なハッシュに切替。SipHash より高速で
    /// allocation/バケット overhead も小さい。
    index: hashbrown::HashMap<String, u32, ahash::RandomState>,
}

impl Default for StringPool {
    fn default() -> Self {
        Self {
            strings: Vec::new(),
            index: hashbrown::HashMap::with_hasher(ahash::RandomState::new()),
        }
    }
}

impl StringPool {
    fn new() -> Self {
        Self::default()
    }

    /// 文字列を登録し ID を返す。既存の文字列は再利用される。
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        let owned = s.to_string();
        self.strings.push(owned.clone());
        self.index.insert(owned, id);
        id
    }

    /// ID に対応する文字列を返す。
    fn get(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }
}

/// 1 caller における (sym_ix, sym_name_id) のリスト。実測的に 1-2 件が大半のため
/// `SmallVec` で inline 保持して `Vec` の heap allocation を回避する。
type SymEntries = smallvec::SmallVec<[(u32, u32); 2]>;

/// Pass 2 内部用。caller_map の key と value を interned ID で保持する中間表現。
/// `hashbrown + ahash` を採用して `u32` key のバケット overhead を削減する。
///   key:   (path_id, line)
///   value: (caller_name_id, SymEntries)
type TypedCallerMap = hashbrown::HashMap<(u32, usize), (u32, SymEntries), ahash::RandomState>;

/// `TypedCallerMap` の空インスタンスを生成する（`ahash` state を明示初期化）。
fn new_typed_caller_map() -> TypedCallerMap {
    hashbrown::HashMap::with_hasher(ahash::RandomState::new())
}

/// 1 チャンクの処理単位。大規模リポジトリで worker local state が肥大化するのを防ぐため、
/// ファイルを `CHUNK_SIZE` 件ずつに区切り、各チャンクの fold/reduce が終わったら
/// ただちに global accumulator に merge して chunk local state を drop する。
/// 128 は「chunk 中の一時データ + reduce 2x でも数百 MB 以内」を狙った保守的な値。
const CHUNK_SIZE: usize = 128;

/// impact streaming Pass の並列度。デフォルトは 1 (= fold/reduce のピーク 2x を避けて
/// RSS を最小化)。CI 等で速度優先にしたい場合は `ASTRO_SIGHT_IMPACT_WORKERS` で上書きする。
fn impact_worker_count() -> usize {
    std::env::var("ASTRO_SIGHT_IMPACT_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

/// Pass 2: per-file で tree-sitter parse を 1 回実行し、Definition 集合と References を
/// 同時に集める。References は Stage 1-6 のうち Stage 4b を除くフィルタを per-file で
/// 適用し、その場で per-worker local の `TypedCallerMap` に流す。候補 Vec は保持しない。
///
/// worker local state の肥大化を抑えるため、ファイル群を `CHUNK_SIZE` 件ずつに区切って
/// rayon fold/reduce を動かし、chunk 終了時に global accumulator へ merge → chunk state を
/// drop する。これにより次の chunk 開始時には前回の chunk local は解放済みとなる。
#[allow(clippy::too_many_arguments)]
fn stream_caller_maps_and_defs(
    file_contexts: &[FileContext],
    all_symbol_names: &[String],
    sym_ix: &HashMap<String, usize>,
    method_parent_types: &HashMap<String, String>,
    included_symbols: &HashSet<String>,
    dir: &Path,
) -> (Vec<TypedCallerMap>, Vec<Vec<u32>>, StringPool) {
    let n_sym = all_symbol_names.len();
    let n_fc = file_contexts.len();

    let mut sym_to_fc: Vec<Vec<u32>> = vec![Vec::new(); n_sym];
    for (fc_ix, ctx) in file_contexts.iter().enumerate() {
        for sym in &ctx.affected {
            let sym_key = ci_key(ctx.lang_id, &sym.name);
            if !included_symbols.contains(&sym_key) {
                continue;
            }
            if let Some(&ix) = sym_ix.get(&sym_key) {
                sym_to_fc[ix].push(fc_ix as u32);
            }
        }
    }

    let mut parent_ix_by_sym: Vec<Option<usize>> = vec![None; n_sym];
    for (sym_key, parent_key) in method_parent_types {
        if let (Some(&ix), Some(&pix)) = (sym_ix.get(sym_key), sym_ix.get(parent_key)) {
            parent_ix_by_sym[ix] = Some(pix);
        }
    }

    let empty_result = || {
        (
            (0..n_fc).map(|_| new_typed_caller_map()).collect(),
            vec![Vec::new(); n_sym],
            StringPool::new(),
        )
    };

    let ac = match refs::build_ac_case_insensitive(all_symbol_names) {
        Ok(a) => a,
        Err(_) => return empty_result(),
    };
    let files = match refs::collect_files(dir, None) {
        Ok(f) => f,
        Err(_) => return empty_result(),
    };

    // rayon fold/reduce は worker local 集約 + reduce acc 併存でピーク RSS が 2x まで
    // 膨らむため、デフォルトは 1 worker (= fold バケット 1 個、ピーク 2x も小さい) とする。
    // 並列性を使いたい CI 環境などでは `ASTRO_SIGHT_IMPACT_WORKERS` で上書きできる。
    let worker_limit = impact_worker_count();
    let rayon_pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
    {
        Ok(p) => p,
        Err(_) => return empty_result(),
    };

    // 全 chunk を通して 1 つの StringPool を共有する。workers=1 なら lock 競合は発生しない。
    let string_pool = std::sync::Mutex::new(StringPool::new());

    let init_state = || -> WorkerState {
        WorkerState {
            local_maps: (0..n_fc).map(|_| new_typed_caller_map()).collect(),
            local_def_paths: vec![Vec::new(); n_sym],
            target_cache: LruCache::new(
                NonZeroUsize::new(TARGET_FILE_CACHE_SIZE).expect("cache size is non-zero"),
            ),
            ref_hit: vec![false; n_sym],
            ref_events: Vec::new(),
            def_events: Vec::new(),
        }
    };

    let mut global_maps: Vec<TypedCallerMap> = (0..n_fc).map(|_| new_typed_caller_map()).collect();
    let mut global_defs: Vec<Vec<u32>> = vec![Vec::new(); n_sym];

    for chunk in files.chunks(CHUNK_SIZE) {
        let chunk_state = rayon_pool.install(|| {
            use rayon::prelude::*;
            chunk
                .par_iter()
                .fold(init_state, |mut state, path| {
                    let Some(path_str) = path.to_str() else {
                        return state;
                    };
                    let utf8_path = camino::Utf8Path::new(path_str);

                    // 本 per-file の可変バッファを `ImpactCollector` にまとめて borrow し、
                    // `visit_refs_and_defs_in_file_cb` の内部から callback で直接流す。
                    // `Vec<Vec<SymbolReference>>` のような per-file の中間バッファを作らない。
                    {
                        let WorkerState {
                            local_maps,
                            local_def_paths,
                            target_cache,
                            ref_hit,
                            ref_events,
                            def_events,
                        } = &mut state;
                        let mut collector = ImpactCollector {
                            sym_to_fc: &sym_to_fc,
                            file_contexts,
                            all_symbol_names,
                            parent_ix_by_sym: &parent_ix_by_sym,
                            pool: &string_pool,
                            path_str,
                            local_maps: local_maps.as_mut_slice(),
                            local_def_paths: local_def_paths.as_mut_slice(),
                            target_cache,
                            ref_hit: ref_hit.as_mut_slice(),
                            ref_events,
                            def_events,
                        };
                        if refs::visit_refs_and_defs_in_file_cb(
                            all_symbol_names,
                            &ac,
                            utf8_path,
                            &mut collector,
                        )
                        .is_ok()
                        {
                            collector.finish_file();
                        } else {
                            // visit に失敗した場合でもバッファだけはクリアして再利用
                            collector.reset_buffers();
                        }
                    }
                    state
                })
                .reduce(init_state, |mut acc, local| {
                    merge_typed_maps(&mut acc.local_maps, local.local_maps);
                    for (acc_v, local_v) in
                        acc.local_def_paths.iter_mut().zip(local.local_def_paths)
                    {
                        acc_v.extend(local_v);
                    }
                    acc
                })
        });

        // chunk 結果を global にマージし、chunk state をスコープ終了で drop させる。
        merge_typed_maps(&mut global_maps, chunk_state.local_maps);
        for (g, c) in global_defs.iter_mut().zip(chunk_state.local_def_paths) {
            g.extend(c);
        }
    }

    let pool = string_pool
        .into_inner()
        .expect("string pool mutex poisoned on unwrap");
    (global_maps, global_defs, pool)
}

/// per-worker の中間状態。per-file バッファ (`ref_hit` / `ref_events` / `def_events`) は
/// `finish_file` / `reset_buffers` で再利用されるため、巨大ファイルでも再割当ては発生しない。
struct WorkerState {
    local_maps: Vec<TypedCallerMap>,
    local_def_paths: Vec<Vec<u32>>,
    target_cache: LruCache<String, Option<ParsedFile>>,
    ref_hit: Vec<bool>,
    ref_events: Vec<RefEventMini>,
    def_events: Vec<u32>,
}

/// 1 件の reference event を最小サイズで表現する。
///
/// `on_ref` 時点で import 判定と caller_name 抽出・intern まで済ませておき、
/// `context` 文字列自体は保持しない（per-file buffer の heap を劇的に削減）。
/// sym_ix / line / column / caller_name_id / is_import_flag の計 24 B 構造体 + 1 bit。
struct RefEventMini {
    sym_ix: u32,
    line: u32,
    column: u32,
    caller_name_id: u32,
    is_import: bool,
}

/// `RefVisitor` の実装: per-file の ref 走査中は最小限の buffering だけ行い、
/// ファイル走査完了後に `finish_file` で Stage 1-6 (Stage 4b 除く) の filter を適用して
/// `local_maps` / `local_def_paths` へ流す。`SymbolReference` の Vec は生成しない。
struct ImpactCollector<'a> {
    sym_to_fc: &'a [Vec<u32>],
    file_contexts: &'a [FileContext],
    all_symbol_names: &'a [String],
    parent_ix_by_sym: &'a [Option<usize>],
    pool: &'a std::sync::Mutex<StringPool>,
    path_str: &'a str,

    local_maps: &'a mut [TypedCallerMap],
    local_def_paths: &'a mut [Vec<u32>],
    target_cache: &'a mut LruCache<String, Option<ParsedFile>>,

    ref_hit: &'a mut [bool],
    ref_events: &'a mut Vec<RefEventMini>,
    def_events: &'a mut Vec<u32>,
}

impl<'a> refs::RefVisitor for ImpactCollector<'a> {
    fn on_ref(&mut self, sym_ix: u32, line: usize, column: usize, context: &str, is_def: bool) {
        let ix = sym_ix as usize;
        if ix < self.ref_hit.len() {
            self.ref_hit[ix] = true;
        }
        if is_def {
            self.def_events.push(sym_ix);
            return;
        }

        // Stage 6 (import 行) の判定は文字列のままでないと行えないため、ここで即決する。
        // caller_name も context から抽出し、pool へ intern して ID にしてから push する。
        // これにより `RefEventMini` は固定長で済み、per-file バッファの heap を削減する。
        let is_import = is_import_context(Some(context));
        let caller_name_fallback = || self.all_symbol_names.get(ix).cloned().unwrap_or_default();
        let caller_name =
            extract_function_from_context(context).unwrap_or_else(caller_name_fallback);
        let caller_name_id = self
            .pool
            .lock()
            .expect("string pool mutex poisoned")
            .intern(&caller_name);

        self.ref_events.push(RefEventMini {
            sym_ix,
            line: line as u32,
            column: column as u32,
            caller_name_id,
            is_import,
        });
    }
}

impl<'a> ImpactCollector<'a> {
    /// ファイル走査完了時に呼ぶ。buffered events に対して Stage 1-6 (Stage 4b 除く) の
    /// filter を適用し、`local_maps` / `local_def_paths` に push する。バッファは clear して
    /// 次ファイルで再利用する。
    fn finish_file(self) {
        // Definition: path を 1 回だけ intern して全 def sym へ配布
        if !self.def_events.is_empty() {
            let path_id = self
                .pool
                .lock()
                .expect("string pool mutex poisoned")
                .intern(self.path_str);
            for &ix in self.def_events.iter() {
                if let Some(paths) = self.local_def_paths.get_mut(ix as usize) {
                    paths.push(path_id);
                }
            }
            self.def_events.clear();
        }

        // References: 1 件ずつ Stage 1-6 (Stage 4b 除く) の filter を適用し local_maps へ流す
        // ref_events を drain することで Vec のヒープは再利用される。Stage 6 (import 判定) と
        // caller_name の抽出は on_ref 時点で済ませてあるため、ここでは flag / ID で判定する。
        for e in self.ref_events.drain(..) {
            if e.is_import {
                continue;
            }

            let sym_ix_usize = e.sym_ix as usize;
            let fc_ixs = &self.sym_to_fc[sym_ix_usize];
            if fc_ixs.is_empty() {
                continue;
            }

            let has_parent_type = self.parent_ix_by_sym[sym_ix_usize].is_some();
            let parent_in_this_file = self.parent_ix_by_sym[sym_ix_usize]
                .and_then(|pix| self.ref_hit.get(pix))
                .copied()
                .unwrap_or(false);

            for &fc_ix_raw in fc_ixs {
                let fc_ix = fc_ix_raw as usize;
                let ctx = &self.file_contexts[fc_ix];
                let source_path = &ctx.new_path;
                let source_lang_group = lang_compat_group(ctx.lang_id);

                if is_same_source_file(self.path_str, source_path) {
                    continue;
                }
                if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(self.path_str))
                    && lang_compat_group(ref_lang) != source_lang_group
                {
                    continue;
                }
                if has_parent_type && !parent_in_this_file {
                    continue;
                }
                if is_ref_in_target_test_context(
                    self.path_str,
                    e.line as usize,
                    e.column as usize,
                    self.target_cache,
                ) {
                    continue;
                }

                let sym_key_canonical = &self.all_symbol_names[sym_ix_usize];
                let affected_sym_name = ctx
                    .affected
                    .iter()
                    .find(|a| ci_key(ctx.lang_id, &a.name) == *sym_key_canonical)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| sym_key_canonical.clone());

                let (path_id, sym_name_id) = {
                    let mut p = self.pool.lock().expect("string pool mutex poisoned");
                    (p.intern(self.path_str), p.intern(&affected_sym_name))
                };

                let ix_u32 = e.sym_ix;
                let key = (path_id, e.line as usize);
                let entry = self.local_maps[fc_ix]
                    .entry(key)
                    .or_insert_with(|| (e.caller_name_id, SymEntries::new()));
                if !entry.1.iter().any(|(existing_ix, existing_name_id)| {
                    *existing_ix == ix_u32 && *existing_name_id == sym_name_id
                }) {
                    entry.1.push((ix_u32, sym_name_id));
                }
            }
        }

        // ref_hit のクリア（次ファイル向け）
        for v in self.ref_hit.iter_mut() {
            *v = false;
        }
    }

    /// visit に失敗したファイルでも buffer だけは空にして次ファイルに備える。
    fn reset_buffers(self) {
        self.ref_events.clear();
        self.def_events.clear();
        for v in self.ref_hit.iter_mut() {
            *v = false;
        }
    }
}

/// 2 つの `Vec<TypedCallerMap>` をエントリ単位で重複排除しつつ merge する。
fn merge_typed_maps(dst: &mut [TypedCallerMap], src: Vec<TypedCallerMap>) {
    for (acc_m, local_m) in dst.iter_mut().zip(src) {
        for (key, (name, entries)) in local_m {
            let entry = acc_m
                .entry(key)
                .or_insert_with(|| (name, SymEntries::new()));
            for (sym_ix_val, sym_name) in entries {
                if !entry.1.iter().any(|(existing_ix, existing_name)| {
                    *existing_ix == sym_ix_val && *existing_name == sym_name
                }) {
                    entry.1.push((sym_ix_val, sym_name));
                }
            }
        }
    }
}

/// Pass 3 (per-fc): 1 つの `TypedCallerMap` に Stage 4b を適用し、interning ID を
/// 剥がした `CallerMap` を返す。呼び出し側で 1 fc_ix ずつ処理し、完了後すぐ drop する
/// ことで、旧実装の `Vec<CallerMap>` 全件同時保持 (0.7-1.2 GB) を回避する。
fn apply_stage4b_single(
    typed_map: TypedCallerMap,
    def_paths_by_ix: &[Vec<u32>],
    pool: &StringPool,
    has_parent_by_ix: &HashMap<u32, bool>,
    source_path: &str,
) -> CallerMap {
    let mut out: CallerMap = HashMap::with_capacity(typed_map.len());
    for ((path_id, line), (caller_name_id, sym_entries)) in typed_map {
        let path_str = pool.get(path_id).to_string();
        let caller_name = pool.get(caller_name_id).to_string();
        let mut kept: Vec<String> = Vec::with_capacity(sym_entries.len());
        for (sym_ix_val, sym_name_id) in sym_entries {
            if *has_parent_by_ix.get(&sym_ix_val).unwrap_or(&false)
                && let Some(paths) = def_paths_by_ix.get(sym_ix_val as usize)
            {
                let has_competing_def = paths.iter().any(|&pid| {
                    let p = pool.get(pid);
                    !is_same_source_file(p, source_path)
                });
                if has_competing_def {
                    continue;
                }
            }
            let sym_name = pool.get(sym_name_id).to_string();
            if !kept.contains(&sym_name) {
                kept.push(sym_name);
            }
        }
        if !kept.is_empty() {
            out.insert((path_str, line), (caller_name, kept));
        }
    }
    out
}

/// Pass 4 (per-fc): 1 ファイル分の `CallerMap` と `FileContext` から `FileImpact` を組み立てる。
/// 同一ファイル内の `call_edges` もここでマージする。
fn build_file_impact(ctx: FileContext, mut caller_map: CallerMap) -> FileImpact {
    // 同一ファイル内の call_edges をマージ
    for sym in &ctx.affected {
        let sym_key = ci_key(ctx.lang_id, &sym.name);
        for edge in &ctx.call_edges {
            if ci_key(ctx.lang_id, &edge.callee.name) == sym_key {
                let caller_line = edge.call_site.line;
                if !ctx
                    .affected
                    .iter()
                    .any(|a| ci_key(ctx.lang_id, &a.name) == ci_key(ctx.lang_id, &edge.caller.name))
                {
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
        .map(|((path, line), (name, mut symbols))| {
            // ahash RandomState 採用で HashMap の iteration 順序が非決定的になるため、
            // 出力の安定性を保つため各 caller 内の symbols もソートする。
            symbols.sort_unstable();
            ImpactedCaller {
                path,
                name,
                line,
                symbols,
            }
        })
        .collect();
    impacted_callers.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));

    FileImpact {
        path: ctx.new_path,
        hunks: ctx.hunks,
        affected_symbols: ctx.affected,
        signature_changes: ctx.sig_changes,
        impacted_callers,
    }
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
