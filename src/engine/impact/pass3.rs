//! Pass 3/4 per-fc 組み立て: Stage 4b 適用と `FileImpact` の最終化。
//!
//! `analyze_impact_streaming` は各 `FileContext` を 1 件ずつ回し、
//!   Pass 3: `apply_stage4b_single` で `TypedCallerMap` → `CallerMap` (String 版) に decode
//!   Pass 4: `build_file_impact` で call_edges マージ + `ImpactedCaller` 整形 → `FileImpact`
//! を実行してすぐ callback に渡す。中間 `Vec<CallerMap>` を保持しないため RSS が節約される。
use std::collections::HashMap;

use crate::models::impact::{FileImpact, ImpactedCaller};

use super::filters::is_same_source_file;
use super::types::{CallerMap, StringPool, TypedCallerMap};
use super::{FileContext, ci_key};

/// method parent type を持つ sym_ix のビットセット相当の map を返す。
/// Stage 4b は method scope のシンボルだけに適用する。
pub(super) fn compute_has_parent_by_ix(
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

/// Pass 3 (per-fc): 1 つの `TypedCallerMap` に Stage 4b (競合 Definition) を適用し、
/// interning ID を剥がした `CallerMap` を返す。呼び出し側で 1 fc_ix ずつ処理し、完了後
/// すぐ drop することで、旧実装の `Vec<CallerMap>` 全件同時保持 (0.7-1.2 GB) を回避する。
pub(super) fn apply_stage4b_single(
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
pub(super) fn build_file_impact(ctx: FileContext, mut caller_map: CallerMap) -> FileImpact {
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
