//! Pass 3/4 per-fc 組み立て: Stage 4b 適用と `FileImpact` の最終化。
//!
//! `analyze_impact_streaming` は各 `FileContext` を 1 件ずつ回し、
//!   Pass 3: `apply_stage4b_single` で `TypedCallerMap` → `CallerMap` (String 版) に decode
//!   Pass 4: `build_file_impact` で call_edges マージ + `ImpactedCaller` 整形 → `FileImpact`
//! を実行してすぐ callback に渡す。中間 `Vec<CallerMap>` を保持しないため RSS が節約される。
use std::collections::{HashMap, HashSet};

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
///
/// `low_caller_map` は Phase 4 で導入された低確信度 caller (BareNameOnly + generic name)。
/// `impacted_callers` を汚染しないよう別フィールドで保持し、出力時には `confidence: "low"`
/// を付与する。
pub(super) fn build_file_impact(
    ctx: FileContext,
    mut caller_map: CallerMap,
    low_caller_map: CallerMap,
) -> FileImpact {
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
                confidence: None,
            }
        })
        .collect();
    impacted_callers.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));

    // Phase 4: 低確信度 caller を `low_confidence_callers` として整形する。
    //
    // 二重計上の意図は「同じ caller 行で同じシンボルが両方に出ないこと」。
    // 旧実装は (path, line) ペア単位で除外していたが、これだと同じ caller 行で
    // 異なるシンボル参照が両方ある場合（例: high `config` + low `new`）に
    // low 側全体が消える誤動作があった (codex 分析)。
    //
    // 正しい dedupe:
    //   - `impacted_callers` 側に同じ (path, line, symbol) があれば low から消す
    //   - 同 (path, line) でも別 symbol なら low に残す
    //
    // 効率のため (path, line) 単位の `HashSet<&String>` を一度作ってから
    // O(1) lookup で retain する。
    let mut impacted_index: HashMap<(&str, usize), HashSet<&str>> = HashMap::new();
    for c in &impacted_callers {
        impacted_index
            .entry((c.path.as_str(), c.line))
            .or_default()
            .extend(c.symbols.iter().map(String::as_str));
    }

    let mut low_confidence_callers: Vec<ImpactedCaller> = low_caller_map
        .into_iter()
        .filter_map(|((path, line), (name, mut symbols))| {
            if let Some(impacted_syms) = impacted_index.get(&(path.as_str(), line)) {
                symbols.retain(|s| !impacted_syms.contains(s.as_str()));
            }
            if symbols.is_empty() {
                return None;
            }
            symbols.sort_unstable();
            Some(ImpactedCaller {
                path,
                name,
                line,
                symbols,
                confidence: Some("low".to_string()),
            })
        })
        .collect();
    low_confidence_callers.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));

    FileImpact {
        path: ctx.new_path,
        hunks: ctx.hunks,
        affected_symbols: ctx.affected,
        signature_changes: ctx.sig_changes,
        impacted_callers,
        low_confidence_callers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    fn empty_ctx(path: &str) -> FileContext {
        FileContext {
            new_path: path.to_string(),
            lang_id: LangId::Php,
            affected: Vec::new(),
            sig_changes: Vec::new(),
            hunks: Vec::new(),
            call_edges: Vec::new(),
            cross_file_symbol_keys: std::collections::HashSet::new(),
        }
    }

    fn caller_map(entries: Vec<(&str, usize, &str, Vec<&str>)>) -> CallerMap {
        let mut m: CallerMap = HashMap::new();
        for (p, l, caller, syms) in entries {
            m.insert(
                (p.to_string(), l),
                (
                    caller.to_string(),
                    syms.into_iter().map(String::from).collect(),
                ),
            );
        }
        m
    }

    /// 同 (path, line) の caller に異なるシンボル参照がある場合、
    /// high 側に存在しない low シンボルは保持されること。
    /// 旧実装は (path, line) ペア単位で除外していたため、
    /// 同行の `config` (high) があるだけで `new` (low) も消えていた。
    #[test]
    fn build_file_impact_keeps_low_when_different_symbol_at_same_line() {
        let ctx = empty_ctx("src/Caller.php");
        let high = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["config"])]);
        let low = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["new"])]);

        let imp = build_file_impact(ctx, high, low);

        assert_eq!(imp.impacted_callers.len(), 1);
        assert_eq!(imp.impacted_callers[0].symbols, vec!["config".to_string()]);
        assert_eq!(imp.low_confidence_callers.len(), 1);
        assert_eq!(
            imp.low_confidence_callers[0].symbols,
            vec!["new".to_string()]
        );
        assert_eq!(
            imp.low_confidence_callers[0].confidence.as_deref(),
            Some("low")
        );
    }

    /// 同 (path, line) で同じシンボル名が high/low 両方にある場合は
    /// low 側から除外されること（強い信号を優先）。
    #[test]
    fn build_file_impact_drops_low_when_same_symbol_at_same_line() {
        let ctx = empty_ctx("src/Caller.php");
        let high = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["new"])]);
        let low = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["new"])]);

        let imp = build_file_impact(ctx, high, low);

        assert_eq!(imp.impacted_callers.len(), 1);
        assert_eq!(imp.impacted_callers[0].symbols, vec!["new".to_string()]);
        assert!(imp.low_confidence_callers.is_empty());
    }

    /// 同 (path, line) の low caller で symbols が一部だけ high と被っているとき、
    /// 被っていないシンボルだけが low に残ること。
    #[test]
    fn build_file_impact_drops_only_overlapping_symbols_from_low() {
        let ctx = empty_ctx("src/Caller.php");
        let high = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["new"])]);
        let low = caller_map(vec![(
            "src/Caller.php",
            42,
            "doStuff",
            vec!["new", "update"],
        )]);

        let imp = build_file_impact(ctx, high, low);

        assert_eq!(imp.impacted_callers.len(), 1);
        assert_eq!(imp.impacted_callers[0].symbols, vec!["new".to_string()]);
        assert_eq!(imp.low_confidence_callers.len(), 1);
        assert_eq!(
            imp.low_confidence_callers[0].symbols,
            vec!["update".to_string()]
        );
    }

    /// high 側に caller がない場合、low 側がそのまま保持されること。
    #[test]
    fn build_file_impact_keeps_low_when_no_high() {
        let ctx = empty_ctx("src/Caller.php");
        let high: CallerMap = HashMap::new();
        let low = caller_map(vec![("src/Caller.php", 42, "doStuff", vec!["new"])]);

        let imp = build_file_impact(ctx, high, low);

        assert!(imp.impacted_callers.is_empty());
        assert_eq!(imp.low_confidence_callers.len(), 1);
        assert_eq!(
            imp.low_confidence_callers[0].symbols,
            vec!["new".to_string()]
        );
    }
}
