//! Pass 2: per-file streaming 収集 (fold/reduce) の実装。
//!
//! `stream_caller_maps_and_defs` が tree-sitter parse を 1 回だけ実行し、
//! `ImpactCollector`（`refs::RefVisitor` 実装）で Definition 集合と References を同時に集め、
//! その場で `TypedCallerMap` にフィルタ適用して流す。worker local state は `CHUNK_SIZE` 件
//! ずつに区切って rayon fold/reduce で処理し、各 chunk の終了時に global accumulator へ
//! merge → chunk state を drop することで RSS ピークを抑える。
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;

use lru::LruCache;

use crate::engine::refs;

use super::collector::{ImpactCollector, WorkerState};
use super::types::{StringPool, SymEntries, TypedCallerMap, new_typed_caller_map};
use super::{FileContext, TARGET_FILE_CACHE_SIZE, ci_key};

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
pub(super) fn stream_caller_maps_and_defs(
    file_contexts: &[FileContext],
    all_symbol_names: &[String],
    sym_ix: &HashMap<String, usize>,
    method_parent_types: &HashMap<String, String>,
    included_symbols: &HashSet<String>,
    dir: &Path,
) -> (
    Vec<TypedCallerMap>,
    Vec<TypedCallerMap>,
    Vec<Vec<u32>>,
    StringPool,
) {
    let n_sym = all_symbol_names.len();
    let n_fc = file_contexts.len();

    // case-insensitive 言語 (Xojo 等) のみで構成された diff では cross-file 解析を
    // skip する。
    //
    // CI 言語は識別子の大文字小文字を区別しないため、`Foo` / `foo` / `FOO` がすべて
    // 同じシンボルになる。これに加えて Xojo 系プロジェクトでは汎用名 (`e` / `row` /
    // `setting` / イベント引数等) が多用される。結果として、cross-file 影響分析は
    // (1) 同名シンボルの per-reference fanout で `global_maps` が処理時間に線形蓄積
    //     して RSS が無制限に膨らむ
    // (2) ノイズだらけで実用的な精度が出ない
    // という二重の問題を抱える。CI 言語のみの diff では cross-file 解析を行わず、
    // 空の結果を返すことで上記を回避する。
    //
    // 強制的に従来挙動に戻したい場合は `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` を
    // 設定する。
    let all_ci = !file_contexts.is_empty()
        && file_contexts
            .iter()
            .all(|fc| fc.lang_id.is_case_insensitive());
    let force = std::env::var("ASTRO_SIGHT_FORCE_CI_LANG_IMPACT")
        .ok()
        .as_deref()
        == Some("1");
    crate::commands::log_phase(
        &format!(
            "context.pass2.ci_skip all_ci={all_ci} force={force} fc_langs={:?}",
            file_contexts
                .iter()
                .map(|fc| fc.lang_id)
                .collect::<Vec<_>>()
        ),
        "info",
        0,
    );
    if all_ci && !force {
        crate::commands::log_phase("context.pass2.ci_skip", "applied", 0);
        return (
            (0..n_fc).map(|_| new_typed_caller_map()).collect(),
            (0..n_fc).map(|_| new_typed_caller_map()).collect(),
            vec![Vec::new(); n_sym],
            StringPool::new(),
        );
    }

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
            local_low_maps: (0..n_fc).map(|_| new_typed_caller_map()).collect(),
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
    let mut global_low_maps: Vec<TypedCallerMap> =
        (0..n_fc).map(|_| new_typed_caller_map()).collect();
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
                            local_low_maps,
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
                            local_low_maps: local_low_maps.as_mut_slice(),
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
                    merge_typed_maps(&mut acc.local_low_maps, local.local_low_maps);
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
        merge_typed_maps(&mut global_low_maps, chunk_state.local_low_maps);
        for (g, c) in global_defs.iter_mut().zip(chunk_state.local_def_paths) {
            g.extend(c);
        }
    }

    let pool = string_pool
        .into_inner()
        .expect("string pool mutex poisoned on unwrap");
    (global_maps, global_low_maps, global_defs, pool)
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
