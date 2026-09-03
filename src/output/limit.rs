//! 出力レコード数の上限適用 (presentation layer)。
//!
//! [`crate::models::result_summary`] が「何を申告するか」を定義するのに対し、こちらは
//! 「実際に何件まで出すか」を決める。判定は **実際に描画したテキスト**に対して行うため、
//! `--format json|toon|auto` と `--pretty` の選択がそのまま予算に反映される
//! (TOON が選ばれれば同じ予算でより多く出せる)。
//!
//! 選択方式は **source order の先頭 N 件** (prefix) で固定する。「定義行を必ず含める」
//! 「ファイルごとに最低 1 件」のような救済を入れると、省略集合が単なる suffix でなくなり
//! (a) rollup の正確な集計が難しくなり (b) 何が出るかの説明可能性が落ちる。
//! `refs` は AST の完全一致なので「関連度」という概念が無く、prefix が最も素直で堅牢。

use anyhow::Result;

use crate::models::reference::{RefsResult, SymbolReference};
use crate::models::result_summary::{
    ResultLimitKind, ResultLimits, ResultSummary, RollupRecord, build_rollup,
};
use crate::output::estimated_tokens;

/// 予算超過時に 1 回のループで削る件数の下限。0 だと収束しない。
const MIN_SHRINK_STEP: usize = 1;

/// 予算計算の暴走を止める上限。実測では 5 回以内に収束する。
const MAX_SHRINK_ITERATIONS: usize = 64;

/// レコード列に上限を適用し、`(出力件数, サマリ)` を返す。
///
/// `render` は「先頭 `k` 件 + サマリ」で実際に出力されるテキストを組み立てるクロージャ。
/// 予算判定はその戻り値の推定トークン数 ([`estimated_tokens`]) に対して行う。
///
/// `complete_input` には「`records` が解析できた入力すべてを数えているか」を渡す
/// (入力側の打ち切りや読み込み失敗があれば false)。`total` の意味づけに使う。
///
/// 返る `shown` が 0 になることは許容する。予算が極端に小さい場合に「最低 1 件は返す」と
/// すると予算そのものが守られなくなるため。CLI 側は `--token-budget` に下限
/// ([`crate::models::result_summary::MIN_TOKEN_BUDGET`]) を課してこの状況を避ける。
pub fn apply_limits<T, F>(
    records: &[T],
    limits: ResultLimits,
    complete_input: bool,
    mut render: F,
) -> Result<(usize, Option<ResultSummary>)>
where
    T: RollupRecord,
    F: FnMut(usize, Option<&ResultSummary>) -> Result<String>,
{
    let total = records.len();
    if limits.is_unlimited() {
        return Ok((total, None));
    }

    // 1. まず件数上限を適用する。
    let capped_by_count = limits.max_results.map_or(total, |m| m.min(total));
    let mut shown = capped_by_count;

    // 2. 予算上限を適用する。サマリの大きさは `shown` に依存する (省略分の rollup が
    //    変わる) ため、描画 → 超過量から次の候補件数を見積もる、を収束するまで繰り返す。
    //    削減幅は必ず 1 件以上とるので、遅くとも shown == 0 で停止する。
    if let Some(budget) = limits.token_budget {
        for _ in 0..MAX_SHRINK_ITERATIONS {
            let summary = build_summary(records, shown, limits, complete_input, capped_by_count);
            let text = render(shown, summary.as_ref())?;
            let size = estimated_tokens(&text);
            if size <= budget || shown == 0 {
                break;
            }
            let over = size - budget;
            // 1 件あたりの平均コストから必要な削減件数を見積もる。
            let per_record = size / shown.max(1);
            let step = (over / per_record.max(1)).max(MIN_SHRINK_STEP);
            shown = shown.saturating_sub(step);
        }
    }

    let summary = build_summary(records, shown, limits, complete_input, capped_by_count);
    Ok((shown, summary))
}

/// 複数グループ (`refs --names a,b,c`) に **1 つの予算**を公平に配分する。
///
/// 配分は入力順の round-robin で、各グループ内は source order の prefix。素直に
/// 「グループごとに `max_results`」にすると全体の上限が名前数に比例して膨らみ、
/// 逆に「先頭グループから順に詰める」と 1 つの高頻度な名前が予算を食い尽くして
/// 後続の名前が 0 件になる (飢餓)。round-robin は決定的で、かつ予算が名前数以上
/// あれば全ての名前に最低 1 件を保証する。
///
/// 返り値は `(グループごとの出力件数, グループごとのサマリ)`。
pub fn apply_grouped_limits<T, F>(
    groups: &[&[T]],
    limits: ResultLimits,
    complete_input: bool,
    mut render: F,
) -> Result<(Vec<usize>, Vec<Option<ResultSummary>>)>
where
    T: RollupRecord,
    F: FnMut(&[usize], &[Option<ResultSummary>]) -> Result<String>,
{
    let counts: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    let total: usize = counts.iter().sum();

    if limits.is_unlimited() {
        return Ok((counts, vec![None; groups.len()]));
    }

    let capped_by_count = limits.max_results.map_or(total, |m| m.min(total));
    let mut budget_slots = capped_by_count;

    // 「件数上限だけを適用したときの配分」。各グループのサマリで
    // 「件数上限で切れたのか、呼び出し全体の予算でさらに切れたのか」を区別する基準線に使う。
    let count_only = round_robin_allocate(&counts, capped_by_count);

    let summaries_for = |alloc: &[usize]| -> Vec<Option<ResultSummary>> {
        groups
            .iter()
            .enumerate()
            .map(|(i, group)| build_summary(group, alloc[i], limits, complete_input, count_only[i]))
            .collect()
    };

    if let Some(budget) = limits.token_budget {
        for _ in 0..MAX_SHRINK_ITERATIONS {
            let alloc = round_robin_allocate(&counts, budget_slots);
            let summaries = summaries_for(&alloc);
            let text = render(&alloc, &summaries)?;
            let size = estimated_tokens(&text);
            if size <= budget || budget_slots == 0 {
                break;
            }
            let over = size - budget;
            let per_record = size / budget_slots.max(1);
            let step = (over / per_record.max(1)).max(MIN_SHRINK_STEP);
            budget_slots = budget_slots.saturating_sub(step);
        }
    }

    let alloc = round_robin_allocate(&counts, budget_slots);
    let summaries = summaries_for(&alloc);
    Ok((alloc, summaries))
}

/// compact JSON を前提に `RefsResult` 単体へ上限を適用する。
///
/// session (NDJSON 固定) と MCP (tool result の text) が共有する経路。どちらも
/// 出力は compact JSON なので、予算判定もその描画に対して行う。
pub fn apply_refs_limits_json(result: &mut RefsResult, limits: ResultLimits) -> Result<()> {
    let complete_input = result.skipped.is_none();
    let references = std::mem::take(&mut result.references);
    let (shown, summary) = apply_limits(&references, limits, complete_input, |k, summary| {
        let probe = RefsResult {
            symbol: result.symbol.clone(),
            references: references[..k].to_vec(),
            skipped: result.skipped.clone(),
            result_summary: summary.cloned(),
        };
        Ok(serde_json::to_string(&probe)?)
    })?;
    let mut rendered = references;
    rendered.truncate(shown);
    result.references = rendered;
    result.result_summary = summary;
    Ok(())
}

/// compact JSON を前提に `refs` バッチ結果へ上限を適用する (呼び出し全体で 1 予算)。
pub fn apply_refs_batch_limits_json(
    results: &mut [RefsResult],
    limits: ResultLimits,
) -> Result<()> {
    let complete_input = results.iter().all(|r| r.skipped.is_none());
    let groups: Vec<&[SymbolReference]> = results.iter().map(|r| r.references.as_slice()).collect();
    let (alloc, summaries) =
        apply_grouped_limits(&groups, limits, complete_input, |alloc, summaries| {
            let capped: Vec<RefsResult> = results
                .iter()
                .enumerate()
                .map(|(i, r)| RefsResult {
                    symbol: r.symbol.clone(),
                    references: r.references[..alloc[i]].to_vec(),
                    skipped: r.skipped.clone(),
                    result_summary: summaries[i].clone(),
                })
                .collect();
            Ok(serde_json::to_string(&capped)?)
        })?;
    for (i, result) in results.iter_mut().enumerate() {
        result.references.truncate(alloc[i]);
        result.result_summary = summaries[i].clone();
    }
    Ok(())
}

/// 各グループへ 1 件ずつ順に配りながら `budget` スロットを使い切る。
///
/// 決定的 (入力順にしか依存しない) で、余りが出れば使い切るまで周回する。
fn round_robin_allocate(counts: &[usize], budget: usize) -> Vec<usize> {
    let mut alloc = vec![0usize; counts.len()];
    let mut remaining = budget;
    while remaining > 0 {
        let mut progressed = false;
        for (slot, &available) in alloc.iter_mut().zip(counts) {
            if remaining == 0 {
                break;
            }
            if *slot < available {
                *slot += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    alloc
}

/// `shown` 件を出したときのサマリを組み立てる。省略が無ければ `None`。
fn build_summary<T: RollupRecord>(
    records: &[T],
    shown: usize,
    limits: ResultLimits,
    complete_input: bool,
    capped_by_count: usize,
) -> Option<ResultSummary> {
    let total = records.len();
    if shown >= total {
        return None;
    }
    let rollup = build_rollup(&records[shown..], limits.token_budget);

    // どの上限が実際に効いたかを両方申告する。件数上限で切れた位置より更に下がって
    // いれば予算側も効いている。
    let mut limited_by = Vec::new();
    if limits.max_results.is_some() && capped_by_count < total {
        limited_by.push(ResultLimitKind::MaxResults);
    }
    if limits.token_budget.is_some() && shown < capped_by_count {
        limited_by.push(ResultLimitKind::TokenBudget);
    }
    // 上限が設定されていない経路では到達しないが、防御的に空を避ける。
    if limited_by.is_empty() {
        limited_by.push(ResultLimitKind::MaxResults);
    }
    limited_by.sort();

    Some(ResultSummary {
        shown,
        total,
        omitted: total - shown,
        limited_by,
        limits,
        complete_input,
        by_kind: rollup.by_kind,
        by_lang: rollup.by_lang,
        files: rollup.files,
        other_files: rollup.other_files,
        rollup_truncated: rollup.rollup_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec {
        path: String,
        kind: &'static str,
    }

    impl RollupRecord for Rec {
        fn rollup_path(&self) -> &str {
            &self.path
        }
        fn rollup_kind(&self) -> Option<&'static str> {
            Some(self.kind)
        }
    }

    fn recs(n: usize) -> Vec<Rec> {
        (0..n)
            .map(|i| Rec {
                path: format!("src/f{:03}.rs", i % 7),
                kind: if i == 0 { "def" } else { "ref" },
            })
            .collect()
    }

    /// 1 レコード = 10 トークン相当の素朴な描画。サマリは 50 トークン相当とする。
    ///
    /// 予算判定は [`estimated_tokens`] (改行なしなら `文字数 / 3` の切り上げ) なので、
    /// 「1 トークン = 3 文字」で文字数を置く。
    fn render_fixed(shown: usize, summary: Option<&ResultSummary>) -> Result<String> {
        let mut s = "x".repeat(shown * 30);
        if summary.is_some() {
            s.push_str(&"y".repeat(150));
        }
        Ok(s)
    }

    /// 上限に達しなければサマリを作らない = 出力は従来と同一。
    #[test]
    fn no_summary_when_under_limits() {
        let records = recs(10);
        let (shown, summary) =
            apply_limits(&records, ResultLimits::DEFAULT, true, render_fixed).expect("apply");
        assert_eq!(shown, 10);
        assert!(summary.is_none());
    }

    /// 無制限指定では全件出し、サマリも作らない。
    #[test]
    fn unlimited_shows_everything() {
        let records = recs(5_000);
        let (shown, summary) =
            apply_limits(&records, ResultLimits::UNLIMITED, true, render_fixed).expect("apply");
        assert_eq!(shown, 5_000);
        assert!(summary.is_none());
    }

    /// 件数上限だけが効いたときは `limited_by` が max_results のみ。
    #[test]
    fn max_results_limit_is_reported_alone() {
        let records = recs(500);
        let limits = ResultLimits {
            max_results: Some(100),
            token_budget: None,
        };
        let (shown, summary) = apply_limits(&records, limits, true, render_fixed).expect("apply");
        assert_eq!(shown, 100);
        let s = summary.expect("summary");
        assert_eq!(s.limited_by, vec![ResultLimitKind::MaxResults]);
        assert_eq!(s.total, 500);
        assert_eq!(s.omitted, 400);
        assert_eq!(s.by_kind.get("ref"), Some(&400));
        assert!(s.complete_input);
    }

    /// 予算が更に厳しいときは両方の理由を申告し、実際に予算内へ収める。
    #[test]
    fn token_budget_shrinks_below_max_results_and_reports_both() {
        let records = recs(500);
        let limits = ResultLimits {
            max_results: Some(100),
            token_budget: Some(300),
        };
        let (shown, summary) = apply_limits(&records, limits, true, render_fixed).expect("apply");
        // 1 件 10 + サマリ 50 なので 25 件で 300。
        assert!(shown <= 25, "shown={shown}");
        let text = render_fixed(shown, summary.as_ref()).expect("render");
        assert!(
            estimated_tokens(&text) <= 300,
            "budget exceeded: {}",
            text.len()
        );
        let s = summary.expect("summary");
        assert_eq!(
            s.limited_by,
            vec![ResultLimitKind::MaxResults, ResultLimitKind::TokenBudget]
        );
    }

    /// 予算がサマリすら収まらない場合は 0 件でも返す (予算を破らない)。
    #[test]
    fn zero_shown_is_allowed_when_budget_cannot_fit_a_record() {
        let records = recs(50);
        let limits = ResultLimits {
            max_results: None,
            token_budget: Some(55),
        };
        let (shown, summary) = apply_limits(&records, limits, true, render_fixed).expect("apply");
        assert_eq!(shown, 0);
        let s = summary.expect("summary");
        assert_eq!(s.omitted, 50);
        assert_eq!(s.limited_by, vec![ResultLimitKind::TokenBudget]);
    }

    /// 入力が不完全なら `complete_input` が false になり、`total` の意味づけが伝わる。
    #[test]
    fn incomplete_input_is_declared() {
        let records = recs(500);
        let (_, summary) =
            apply_limits(&records, ResultLimits::DEFAULT, false, render_fixed).expect("apply");
        assert!(!summary.expect("summary").complete_input);
    }

    /// 同じ入力に対して結果が決定的である (rollup の順序も含めて)。
    #[test]
    fn output_is_deterministic() {
        let records = recs(500);
        let first = apply_limits(&records, ResultLimits::DEFAULT, true, render_fixed).expect("a");
        for _ in 0..5 {
            let again =
                apply_limits(&records, ResultLimits::DEFAULT, true, render_fixed).expect("b");
            assert_eq!(first.0, again.0);
            assert_eq!(first.1, again.1);
        }
    }
}
