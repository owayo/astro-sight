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
/// ([`crate::models::result_summary::MIN_TOKEN_BUDGET`]) を課し、`refs` は申告の代表パスを
/// 参照より先に縮める ([`apply_refs_limits`])。それでも件数・申告の本文・rollup のような
/// 縮められない部分が予算を超えれば、0 件まで下がって `budget_exceeded` を申告する。
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
    let capped_by_count = count_capped(total, limits);
    let mut shown = capped_by_count;

    // 2. 予算上限を適用する。サマリの大きさは `shown` に依存する (省略分の rollup が
    //    変わる) ため、描画 → 超過量から次の候補件数を見積もる、を収束するまで繰り返す。
    //    削減幅は必ず 1 件以上とるので、遅くとも shown == 0 で停止する。
    if let Some(budget) = limits.token_budget {
        for _ in 0..MAX_SHRINK_ITERATIONS {
            let summary = build_summary(
                records,
                shown,
                limits,
                complete_input,
                capped_by_count,
                limits.token_budget,
            );
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

    let mut summary = build_summary(
        records,
        shown,
        limits,
        complete_input,
        capped_by_count,
        limits.token_budget,
    );
    // 最小まで絞っても予算に収まらなかったなら、そのことを申告する
    // (無言で超過すると「予算どおりに返った」と読まれる)。
    if let (Some(budget), Some(sum)) = (limits.token_budget, summary.as_mut()) {
        let text = render(shown, Some(sum))?;
        if estimated_tokens(&text) > budget {
            sum.budget_exceeded = true;
        }
    }
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
    if limits.is_unlimited() {
        let counts = groups.iter().map(|g| g.len()).collect();
        return Ok((counts, vec![None; groups.len()]));
    }

    let plan = GroupedCountPlan::new(groups, limits);
    let mut budget_slots = plan.capped_by_count;

    if let Some(budget) = limits.token_budget {
        for _ in 0..MAX_SHRINK_ITERATIONS {
            let alloc = round_robin_allocate(&plan.counts, budget_slots);
            let summaries = plan.summaries(groups, &alloc, limits, complete_input);
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

    let alloc = round_robin_allocate(&plan.counts, budget_slots);
    let mut summaries = plan.summaries(groups, &alloc, limits, complete_input);
    // 予算に収まらなかったことは呼び出し全体の性質なので、出力される全サマリに
    // 同じ値を立てる (NDJSON はグループごとに 1 行で、呼び出し全体を表す場所が無い)。
    if let Some(budget) = limits.token_budget {
        let text = render(&alloc, &summaries)?;
        if estimated_tokens(&text) > budget {
            for summary in summaries.iter_mut().flatten() {
                summary.budget_exceeded = true;
            }
        }
    }
    Ok((alloc, summaries))
}

/// `RefsResult` 単体へ上限を適用する。
///
/// `render` には出力面が**実際に使う描画**を渡す (CLI は `--format` / `--pretty` に従う
/// 単一ドキュメント、MCP は tool result の text、session は compact JSON)。予算判定は
/// その描画結果に対して行うため、描画と採寸がずれて特定の形式でだけ黙って予算を超える、
/// ということが起きない (旧実装は MCP の採寸を compact JSON に固定しており、`--pretty` /
/// `--format toon` の MCP が予算の 2 倍前後を `budget_exceeded` なしで返していた)。
///
/// `complete_input` は生成物の除外 (`skipped`) と読み込み・parse 失敗の両方から決める
/// ([`RefsResult::input_is_complete`])。
///
/// 参照を削る前に、申告 (`skipped` / `truncations`) の代表パスを縮める
/// ([`fit_declaration_samples`])。代表パスが長いと、申告だけで予算を使い切って参照が
/// 0 件まで削られていたため。
pub fn apply_refs_limits<F>(
    result: &mut RefsResult,
    limits: ResultLimits,
    mut render: F,
) -> Result<()>
where
    F: FnMut(&RefsResult) -> Result<String>,
{
    let complete_input = result.input_is_complete();
    let references = std::mem::take(&mut result.references);
    fit_declaration_samples(result, &references, limits, complete_input, &mut render)?;
    let (shown, summary) = apply_limits(&references, limits, complete_input, |k, summary| {
        let probe = RefsResult {
            symbol: result.symbol.clone(),
            references: references[..k].to_vec(),
            skipped: result.skipped.clone(),
            truncations: result.truncations.clone(),
            result_summary: summary.cloned(),
            failed_files: result.failed_files,
        };
        render(&probe)
    })?;
    let mut rendered = references;
    rendered.truncate(shown);
    result.references = rendered;
    result.result_summary = summary;
    Ok(())
}

/// compact JSON で描画する出力面 (session の NDJSON) 向けの [`apply_refs_limits`]。
pub fn apply_refs_limits_json(result: &mut RefsResult, limits: ResultLimits) -> Result<()> {
    apply_refs_limits(result, limits, |probe| Ok(serde_json::to_string(probe)?))
}

/// `refs` バッチ結果へ上限を適用する (呼び出し全体で 1 予算)。
///
/// `render` は上限適用後の結果列を出力面の描画で組み立てるクロージャ
/// ([`apply_refs_limits`] と同じく、採寸と実際の描画を一致させるため)。
pub fn apply_refs_batch_limits<F>(
    results: &mut [RefsResult],
    limits: ResultLimits,
    mut render: F,
) -> Result<()>
where
    F: FnMut(&[RefsResult]) -> Result<String>,
{
    let complete_input = results.iter().all(RefsResult::input_is_complete);
    fit_batch_declaration_samples(results, limits, complete_input, &mut render)?;
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
                    truncations: r.truncations.clone(),
                    result_summary: summaries[i].clone(),
                    failed_files: r.failed_files,
                })
                .collect();
            render(&capped)
        })?;
    for (i, result) in results.iter_mut().enumerate() {
        result.references.truncate(alloc[i]);
        result.result_summary = summaries[i].clone();
    }
    Ok(())
}

/// compact JSON で描画する出力面 (session の NDJSON) 向けの [`apply_refs_batch_limits`]。
pub fn apply_refs_batch_limits_json(
    results: &mut [RefsResult],
    limits: ResultLimits,
) -> Result<()> {
    apply_refs_batch_limits(results, limits, |capped| Ok(serde_json::to_string(capped)?))
}

/// 件数上限までの参照を削らずに済むよう、申告 (`skipped` / `truncations`) の代表パスを
/// 先に縮める。
///
/// 代表パスは入力の欠落を調べる手掛かりで、観測した参照より優先しない。件数上限までの
/// 参照と一緒に予算へ収まる最大の件数 (リストごとに同じ上限) を探し、どの件数でも
/// 収まらなければ 0 件にして、参照の縮小 ([`apply_limits`]) に任せる。件数
/// (`generated` / `detail.count`) は残し、縮めたリストには理由 (`token_budget`) を付ける。
/// 元から予算に収まる出力は変えない。
fn fit_declaration_samples<F>(
    result: &mut RefsResult,
    references: &[SymbolReference],
    limits: ResultLimits,
    complete_input: bool,
    render: &mut F,
) -> Result<()>
where
    F: FnMut(&RefsResult) -> Result<String>,
{
    let max_cap = result.declaration_sample_len();
    let Some(budget) = limits.token_budget else {
        return Ok(());
    };
    if max_cap == 0 {
        return Ok(());
    }
    let shown = count_capped(references.len(), limits);
    let mut probe = RefsResult {
        symbol: result.symbol.clone(),
        references: references[..shown].to_vec(),
        skipped: None,
        truncations: Vec::new(),
        result_summary: build_summary(
            references,
            shown,
            limits,
            complete_input,
            shown,
            limits.token_budget,
        ),
        failed_files: result.failed_files,
    };
    let cap = largest_fitting_sample_cap(max_cap, budget, |cap| {
        (probe.skipped, probe.truncations) =
            result.declarations_with_sample_cap(cap, ResultLimitKind::TokenBudget);
        render(&probe)
    })?
    .unwrap_or(0);
    (result.skipped, result.truncations) =
        result.declarations_with_sample_cap(cap, ResultLimitKind::TokenBudget);
    Ok(())
}

/// [`fit_declaration_samples`] のバッチ版 (呼び出し全体で 1 予算)。
///
/// 件数上限だけを配った状態 ([`GroupedCountPlan`]) で収まる最大の件数を探し、全結果の
/// 申告に同じ上限を掛ける。申告は通常先頭の結果にだけ付くが、それを前提にしない。
fn fit_batch_declaration_samples<F>(
    results: &mut [RefsResult],
    limits: ResultLimits,
    complete_input: bool,
    render: &mut F,
) -> Result<()>
where
    F: FnMut(&[RefsResult]) -> Result<String>,
{
    let max_cap = results
        .iter()
        .map(RefsResult::declaration_sample_len)
        .max()
        .unwrap_or(0);
    let Some(budget) = limits.token_budget else {
        return Ok(());
    };
    if max_cap == 0 {
        return Ok(());
    }
    let groups: Vec<&[SymbolReference]> = results.iter().map(|r| r.references.as_slice()).collect();
    let plan = GroupedCountPlan::new(&groups, limits);
    let summaries = plan.summaries(&groups, &plan.count_only, limits, complete_input);
    let mut probes: Vec<RefsResult> = results
        .iter()
        .zip(summaries)
        .zip(&plan.count_only)
        .map(|((r, summary), &shown)| RefsResult {
            symbol: r.symbol.clone(),
            references: r.references[..shown].to_vec(),
            skipped: None,
            truncations: Vec::new(),
            result_summary: summary,
            failed_files: r.failed_files,
        })
        .collect();
    let cap = largest_fitting_sample_cap(max_cap, budget, |cap| {
        for (probe, result) in probes.iter_mut().zip(results.iter()) {
            (probe.skipped, probe.truncations) =
                result.declarations_with_sample_cap(cap, ResultLimitKind::TokenBudget);
        }
        render(&probes)
    })?
    .unwrap_or(0);
    for result in results.iter_mut() {
        (result.skipped, result.truncations) =
            result.declarations_with_sample_cap(cap, ResultLimitKind::TokenBudget);
    }
    Ok(())
}

/// 申告の代表パスの件数を `max_cap` から 1 件ずつ減らして描画し、予算に収まった最初の
/// 件数を返す。どの件数でも収まらなければ `None`。
///
/// 二分探索にしないのは、描画の大きさが件数について単調だと言えないため
/// (`--format auto` の形式選択と予算判定は別の尺度で、TOON の nullable 補完の採否も
/// 別の尺度で決まる)。上から順に試せば、単調でなくても収まる最大の件数が得られる。
fn largest_fitting_sample_cap<F>(
    max_cap: usize,
    budget: usize,
    mut render: F,
) -> Result<Option<usize>>
where
    F: FnMut(usize) -> Result<String>,
{
    for cap in (0..=max_cap).rev() {
        if estimated_tokens(&render(cap)?) <= budget {
            return Ok(Some(cap));
        }
    }
    Ok(None)
}

/// 件数上限だけを掛けたときの出力件数。
fn count_capped(total: usize, limits: ResultLimits) -> usize {
    limits.max_results.map_or(total, |m| m.min(total))
}

/// グループ版の「件数上限だけを掛けた状態」と、グループごとのサマリの組み立て方。
/// [`apply_grouped_limits`] と [`fit_batch_declaration_samples`] で同じ計算を使う。
struct GroupedCountPlan {
    counts: Vec<usize>,
    capped_by_count: usize,
    /// 件数上限だけを適用したときの配分。各グループのサマリで「件数上限で切れたのか、
    /// 呼び出し全体の予算でさらに切れたのか」を区別する基準線に使う。
    count_only: Vec<usize>,
    /// rollup の採寸に使う予算 (呼び出し全体の予算 ÷ グループ数)。全体予算をそのまま
    /// 各グループへ渡すと rollup がグループ数だけ多重計上され、予算を大幅に超過する。
    per_group_rollup_budget: Option<usize>,
}

impl GroupedCountPlan {
    fn new<T>(groups: &[&[T]], limits: ResultLimits) -> Self {
        let counts: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        let capped_by_count = count_capped(counts.iter().sum(), limits);
        let count_only = round_robin_allocate(&counts, capped_by_count);
        let per_group_rollup_budget = limits
            .token_budget
            .map(|b| (b / groups.len().max(1)).max(1));
        Self {
            counts,
            capped_by_count,
            count_only,
            per_group_rollup_budget,
        }
    }

    /// グループごとに `alloc[i]` 件を出したときのサマリ。
    fn summaries<T: RollupRecord>(
        &self,
        groups: &[&[T]],
        alloc: &[usize],
        limits: ResultLimits,
        complete_input: bool,
    ) -> Vec<Option<ResultSummary>> {
        groups
            .iter()
            .enumerate()
            .map(|(i, group)| {
                build_summary(
                    group,
                    alloc[i],
                    limits,
                    complete_input,
                    self.count_only[i],
                    self.per_group_rollup_budget,
                )
            })
            .collect()
    }
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
/// `limits` は**利用者が指定した値**として出力にそのまま載せる。一方 `rollup_budget` は
/// 「このサマリ 1 個が rollup に使ってよい予算」で、バッチでは呼び出し全体の予算を
/// グループ数で割った値が渡る。
///
/// 分けているのは、旧実装が rollup の採寸にも `limits.token_budget` (呼び出し全体の予算)
/// を使っていたため、`refs --names` でグループ数だけ rollup が多重計上され、
/// **参照 0 件・予算の 5 倍**という出力になっていたため (実測: 10 名前 /
/// `--token-budget 256` で 1,331 tokens)。
fn build_summary<T: RollupRecord>(
    records: &[T],
    shown: usize,
    limits: ResultLimits,
    complete_input: bool,
    capped_by_count: usize,
    rollup_budget: Option<usize>,
) -> Option<ResultSummary> {
    let total = records.len();
    if shown >= total {
        return None;
    }
    let rollup = build_rollup(&records[shown..], rollup_budget);

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
        // 予算超過の申告は呼び出し側 (apply_limits / apply_grouped_limits) が
        // 最終描画を実測してから立てる。
        budget_exceeded: false,
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

    pub(super) struct Rec {
        pub(super) path: String,
        pub(super) kind: &'static str,
    }

    impl RollupRecord for Rec {
        fn rollup_path(&self) -> &str {
            &self.path
        }
        fn rollup_kind(&self) -> Option<&'static str> {
            Some(self.kind)
        }
    }

    pub(super) fn recs(n: usize) -> Vec<Rec> {
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
    pub(super) fn render_fixed(shown: usize, summary: Option<&ResultSummary>) -> Result<String> {
        let mut s = "x".repeat(shown * 30);
        if summary.is_some() {
            s.push_str(&"y".repeat(150));
        }
        Ok(s)
    }

    /// グループ版の素朴な描画。`render_fixed` と同じ縮尺 (1 レコード = 10 トークン、
    /// サマリ 1 個 = 50 トークン) で、rollup のパス長も実測に反映させる
    /// (rollup 予算の分割が効いていることをテストから観測できるようにするため)。
    pub(super) fn render_grouped_fixed(
        alloc: &[usize],
        summaries: &[Option<ResultSummary>],
    ) -> Result<String> {
        let mut s = String::new();
        for (i, shown) in alloc.iter().enumerate() {
            s.push_str(&"x".repeat(shown * 30));
            if let Some(summary) = summaries.get(i).and_then(Option::as_ref) {
                s.push_str(&"y".repeat(150));
                for f in &summary.files {
                    s.push_str(f.path.as_str());
                }
            }
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

#[cfg(test)]
mod budget_tests {
    use super::tests::*;
    use super::*;

    /// 最小まで絞っても予算に収まらないとき、その事実を申告する。
    ///
    /// 申告そのものに固定コスト (サマリ本体) があるため、グループ数 × サマリが
    /// 予算を超えると**表示件数を 0 にしても超過**する。旧実装はこれを無言で返し、
    /// 実測で `refs --names` 10 個 / `--token-budget 256` が
    /// 「参照 0 件・予算の 5.2 倍」になっていた。
    #[test]
    fn grouped_limits_declare_unmeetable_budget() {
        let g0 = recs(50);
        let g1 = recs(50);
        let groups: Vec<&[Rec]> = vec![&g0, &g1];
        // サマリ 1 個 = 50 トークン相当なので、2 グループでは予算 20 は原理的に不可能。
        let limits = ResultLimits {
            max_results: Some(100),
            token_budget: Some(20),
        };
        let (alloc, summaries) =
            apply_grouped_limits(&groups, limits, true, render_grouped_fixed).unwrap();

        assert_eq!(alloc, vec![0, 0], "予算不足なので表示件数は 0 に落ちる");
        for summary in summaries.iter().flatten() {
            assert!(
                summary.budget_exceeded,
                "予算を守れなかったことを申告すべき: {summary:?}"
            );
        }
        assert_eq!(
            summaries.iter().flatten().count(),
            2,
            "両グループが申告する"
        );
    }

    /// 対照: 予算内に収まるなら申告しない (出力が余計に膨らまない)。
    #[test]
    fn grouped_limits_do_not_declare_when_budget_is_met() {
        let g0 = recs(50);
        let g1 = recs(50);
        let groups: Vec<&[Rec]> = vec![&g0, &g1];
        let limits = ResultLimits {
            max_results: Some(100),
            token_budget: Some(400),
        };
        let (alloc, summaries) =
            apply_grouped_limits(&groups, limits, true, render_grouped_fixed).unwrap();

        assert!(alloc.iter().sum::<usize>() > 0, "予算内なら件数が出る");
        for summary in summaries.iter().flatten() {
            assert!(
                !summary.budget_exceeded,
                "予算を守れているので申告しない: {summary:?}"
            );
        }
    }

    /// rollup の採寸は「呼び出し全体の予算 ÷ グループ数」で行う。
    ///
    /// 旧実装は各グループの rollup にも呼び出し全体の予算を渡していたため、
    /// グループ数だけ rollup が多重計上され予算を大幅に超えていた。
    /// グループ数が増えても 1 グループあたりの rollup が膨らまないことを固定する。
    #[test]
    fn grouped_rollup_budget_is_divided_across_groups() {
        let many: Vec<Rec> = (0..200)
            .map(|i| Rec {
                path: format!("src/dir{i:03}/file{i:03}.rs"),
                kind: "ref",
            })
            .collect();
        let limits = ResultLimits {
            max_results: Some(0),
            token_budget: Some(600),
        };

        let single: Vec<&[Rec]> = vec![&many];
        let (_, one) = apply_grouped_limits(&single, limits, true, render_grouped_fixed).unwrap();
        let one_files = one[0].as_ref().unwrap().files.len();

        let eight: Vec<&[Rec]> = vec![&many; 8];
        let (_, many_groups) =
            apply_grouped_limits(&eight, limits, true, render_grouped_fixed).unwrap();
        let each_files = many_groups[0].as_ref().unwrap().files.len();

        assert!(
            each_files < one_files,
            "グループが増えたら 1 グループあたりの rollup は縮むべき: {each_files} < {one_files}"
        );
    }
}

#[cfg(test)]
mod refs_tests {
    use super::*;
    use crate::models::reference::RefKind;

    fn refs_result(n: usize, failed_files: usize) -> RefsResult {
        RefsResult {
            symbol: "hot".into(),
            references: (0..n)
                .map(|i| SymbolReference {
                    path: format!("src/f{}.rs", i % 5),
                    line: i,
                    column: 4,
                    context: Some(format!("hot({i});")),
                    kind: Some(RefKind::Reference),
                    confidence: None,
                })
                .collect(),
            skipped: None,
            truncations: Vec::new(),
            result_summary: None,
            failed_files,
        }
    }

    fn count_limit() -> ResultLimits {
        ResultLimits {
            max_results: Some(10),
            token_budget: None,
        }
    }

    /// 読み込み・parse に失敗したファイルがあれば `complete_input` を false にする。
    /// 旧実装は `skipped` (生成物の除外) しか見ていなかった。
    #[test]
    fn failed_files_make_the_input_incomplete() {
        // 対照: 失敗が無ければ complete_input は true。
        let mut complete = refs_result(50, 0);
        apply_refs_limits_json(&mut complete, count_limit()).expect("apply");
        assert!(complete.result_summary.expect("summary").complete_input);

        let mut single = refs_result(50, 1);
        apply_refs_limits_json(&mut single, count_limit()).expect("apply");
        assert!(!single.result_summary.expect("summary").complete_input);

        // バッチは 1 件でも不完全なら全体が不完全。
        let mut batch = vec![refs_result(50, 0), refs_result(50, 2)];
        apply_refs_batch_limits_json(&mut batch, count_limit()).expect("apply");
        for result in &batch {
            if let Some(summary) = &result.result_summary {
                assert!(!summary.complete_input, "{summary:?}");
            }
        }
        assert!(batch.iter().any(|r| r.result_summary.is_some()));
    }

    /// 対応言語でないソースの申告 (`truncations`) があれば `complete_input` を false にし、
    /// 申告は採寸用の probe にも載せる (載せないと、申告の分だけ予算を黙って超える)。
    #[test]
    fn unanalyzable_sources_make_the_input_incomplete_and_count_toward_the_budget() {
        let truncation = || {
            vec![
                crate::models::truncation::TruncationInfo::unanalyzable_source(
                    "vue",
                    1,
                    &["src/App.vue".to_string()],
                ),
            ]
        };

        // 採寸 (描画) は予算があるときだけ走る。
        let budget = ResultLimits {
            max_results: Some(10),
            token_budget: Some(3000),
        };
        let mut single = refs_result(50, 0);
        single.truncations = truncation();
        let mut probes_with_truncations = 0;
        apply_refs_limits(&mut single, budget, |probe| {
            if !probe.truncations.is_empty() {
                probes_with_truncations += 1;
            }
            Ok(serde_json::to_string(probe)?)
        })
        .expect("apply");
        assert!(probes_with_truncations > 0, "probe が申告を落とした");
        assert!(!single.result_summary.expect("summary").complete_input);
        assert_eq!(single.truncations, truncation(), "申告は出力に残る");

        // バッチの申告は先頭の結果にだけ付くが、後続の名前の summary も不完全になる。
        let mut first = refs_result(50, 0);
        first.truncations = truncation();
        let mut batch = vec![first, refs_result(50, 0)];
        apply_refs_batch_limits_json(&mut batch, count_limit()).expect("apply");
        for result in &batch {
            let summary = result.result_summary.as_ref().expect("summary");
            assert!(!summary.complete_input, "{}: {summary:?}", result.symbol);
        }

        // 対照: 申告が無ければ complete_input は true のまま。
        let mut complete = refs_result(50, 0);
        apply_refs_limits_json(&mut complete, count_limit()).expect("apply");
        assert!(complete.result_summary.expect("summary").complete_input);
    }

    /// 予算は渡した描画で採寸する。描画が重いほど出せる件数は減り、最終描画は予算内に収まる。
    #[test]
    fn budget_is_measured_with_the_given_renderer() {
        let limits = ResultLimits {
            max_results: None,
            token_budget: Some(600),
        };

        let mut compact = refs_result(200, 0);
        apply_refs_limits(&mut compact, limits, |probe| {
            Ok(serde_json::to_string(probe)?)
        })
        .expect("apply");
        let mut pretty = refs_result(200, 0);
        apply_refs_limits(&mut pretty, limits, |probe| {
            Ok(serde_json::to_string_pretty(probe)?)
        })
        .expect("apply");

        let rendered = serde_json::to_string_pretty(&pretty).expect("render");
        assert!(
            estimated_tokens(&rendered) <= 600
                || pretty
                    .result_summary
                    .as_ref()
                    .is_some_and(|s| s.budget_exceeded),
            "pretty の最終描画が予算を申告なしで超えた: {} tokens",
            estimated_tokens(&rendered)
        );
        assert!(
            pretty.references.len() < compact.references.len(),
            "pretty={} compact={}",
            pretty.references.len(),
            compact.references.len()
        );
    }
}

/// 申告 (`skipped` / `truncations`) の代表パスは、参照より先に縮める (Issue #181)。
#[cfg(test)]
mod declaration_sample_tests {
    use super::*;
    use crate::models::reference::RefKind;
    use crate::models::skip::SkippedFiles;
    use crate::models::truncation::TruncationInfo;

    const LONG_DIR: &str =
        "generated/a_rather_long_generated_directory_name/another_nested_package_directory";
    const TOKEN_BUDGET: ResultLimitKind = ResultLimitKind::TokenBudget;

    /// `refs` 件の参照と、生成物 `paths + 10` 件 (代表パス `paths` 件) の申告を持つ結果。
    fn result_with(refs: usize, paths: usize) -> RefsResult {
        RefsResult {
            symbol: "Alpha".into(),
            references: (0..refs)
                .map(|i| SymbolReference {
                    path: format!("src/caller{}.php", i % 3),
                    line: i,
                    column: 5,
                    context: Some(format!("$a{i} = new Alpha();")),
                    kind: Some(RefKind::Reference),
                    confidence: None,
                })
                .collect(),
            skipped: (paths > 0).then(|| SkippedFiles {
                generated: paths + 10,
                paths: (0..paths)
                    .map(|i| format!("{LONG_DIR}/generated_message_type_{i:02}.php"))
                    .collect(),
                truncated: true,
                paths_limited_by: None,
            }),
            truncations: Vec::new(),
            result_summary: None,
            failed_files: 0,
        }
    }

    /// 解析できないソースの申告 (拡張子 10 種 × 長い代表パス 3 件)。
    fn unanalyzable_declarations() -> Vec<TruncationInfo> {
        [
            "astro", "dart", "erb", "groovy", "lua", "mdx", "scala", "svelte", "twig", "vue",
        ]
        .iter()
        .map(|ext| {
            let examples: Vec<String> = (1..=3)
                .map(|i| format!("{LONG_DIR}/component_template_{i}.{ext}"))
                .collect();
            TruncationInfo::unanalyzable_source(ext, 3, &examples)
        })
        .collect()
    }

    fn json(result: &RefsResult) -> Result<String> {
        Ok(serde_json::to_string(result)?)
    }

    fn tokens(result: &RefsResult) -> usize {
        estimated_tokens(&json(result).unwrap())
    }

    /// 参照の列 (`SymbolReference` は `PartialEq` を持たないので出力の形で比べる)。
    fn refs_json(result: &RefsResult) -> serde_json::Value {
        serde_json::to_value(&result.references).unwrap()
    }

    fn budget(tokens: usize) -> ResultLimits {
        ResultLimits {
            max_results: Some(100),
            token_budget: Some(tokens),
        }
    }

    /// `cap` 件に縮めた申告を付けた `original` の描画の大きさ (縮めた理由のフィールドと
    /// 作り直した `message` を含む)。
    fn cost_at(original: &RefsResult, cap: usize) -> usize {
        let mut probe = original.clone();
        (probe.skipped, probe.truncations) =
            original.declarations_with_sample_cap(cap, TOKEN_BUDGET);
        tokens(&probe)
    }

    /// 件数上限までの参照を全部出し、代表パスだけを予算に収まる最大の件数まで縮める。
    ///
    /// 予算をちょうど「2 件に縮めた描画」の大きさにすると 2 件を選ぶ。縮めた理由の
    /// フィールドと作り直した `message` の分まで採寸に含めていないと、この境界で外れる。
    #[test]
    fn samples_shrink_before_any_reference_is_cut() {
        let mut original = result_with(2, 50);
        original.truncations = unanalyzable_declarations();
        let limit = cost_at(&original, 2);
        assert!(cost_at(&original, 3) > limit, "前提: 3 件では収まらない");
        let mut result = original.clone();
        apply_refs_limits_json(&mut result, budget(limit)).unwrap();

        assert_eq!(refs_json(&result), refs_json(&original), "参照は削らない");
        assert!(
            result.result_summary.is_none(),
            "参照の省略が無ければサマリは出さない"
        );
        assert!(tokens(&result) <= limit, "{} > {limit}", tokens(&result));

        let skipped = result.skipped.as_ref().unwrap();
        let original_paths = &original.skipped.as_ref().unwrap().paths;
        assert_eq!(skipped.generated, 60, "全件数は残す");
        assert!(skipped.truncated);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert_eq!(
            skipped.paths[..],
            original_paths[..2],
            "先頭からの prefix を残す"
        );
        for (after, before) in result.truncations.iter().zip(&original.truncations) {
            let (after, before) = (
                after.detail.as_ref().unwrap(),
                before.detail.as_ref().unwrap(),
            );
            assert_eq!(after.count, before.count, "件数は残す");
            assert_eq!(after.examples[..], before.examples[..2]);
            assert_eq!(after.examples_limited_by, Some(TOKEN_BUDGET));
        }
    }

    /// 代表パスのリストごとに同じ上限を掛けるので、短いリスト (代表パス 3 件) は長い
    /// リスト (50 件) が 3 件まで縮むまで元のまま (理由も付けない)。
    #[test]
    fn longer_sample_lists_shrink_first() {
        let mut original = result_with(2, 50);
        original.truncations = unanalyzable_declarations();
        let limit = cost_at(&original, 10);
        let mut result = original.clone();
        apply_refs_limits_json(&mut result, budget(limit)).unwrap();

        assert_eq!(refs_json(&result), refs_json(&original));
        let skipped = result.skipped.as_ref().unwrap();
        assert_eq!(skipped.paths.len(), 10);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert_eq!(
            serde_json::to_value(&result.truncations).unwrap(),
            serde_json::to_value(&original.truncations).unwrap(),
            "縮めていないリストは元のまま"
        );
    }

    /// 対照: 元から予算に収まるなら、申告は元のまま (理由も付けない) で出力はバイト一致。
    #[test]
    fn samples_are_untouched_when_everything_fits() {
        let mut original = result_with(2, 50);
        original.truncations = unanalyzable_declarations();
        for limits in [
            budget(tokens(&original)),
            ResultLimits {
                max_results: Some(100),
                token_budget: None,
            },
            ResultLimits::UNLIMITED,
        ] {
            let mut result = original.clone();
            apply_refs_limits_json(&mut result, limits).unwrap();
            assert_eq!(
                json(&result).unwrap(),
                json(&original).unwrap(),
                "{limits:?}"
            );
        }
    }

    /// 代表パスを 0 件にしても参照が収まらないときだけ参照を削り、代表パスは 0 件のまま。
    #[test]
    fn references_are_cut_only_after_samples_reach_zero() {
        let original = result_with(100, 50);
        let limit = tokens(&result_with(100, 0)) / 2;
        let mut result = original.clone();
        apply_refs_limits_json(&mut result, budget(limit)).unwrap();

        let skipped = result.skipped.as_ref().unwrap();
        assert!(skipped.paths.is_empty(), "{}", skipped.paths.len());
        assert_eq!(skipped.generated, 60);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert!(
            !result.references.is_empty() && result.references.len() < 100,
            "{}",
            result.references.len()
        );
        let summary = result.result_summary.as_ref().unwrap();
        assert_eq!(summary.limited_by, vec![ResultLimitKind::TokenBudget]);
        assert!(!summary.budget_exceeded);
        assert!(tokens(&result) <= limit, "{} > {limit}", tokens(&result));
    }

    /// 描画の大きさが代表パスの件数について単調でなくても、収まる最大の件数を選ぶ。
    /// 0 件で超えても途中の件数を試す (二分探索だと参照の縮小へ進んでしまう)。
    #[test]
    fn non_monotonic_renderer_still_gets_the_largest_fitting_cap() {
        // 代表パスが 3 件のときだけ小さく描画される (形式の切り替えを模す)。
        let render = |probe: &RefsResult| -> Result<String> {
            let paths = probe.skipped.as_ref().map_or(0, |s| s.paths.len());
            let size = if paths == 3 { 100 } else { 1_000 };
            // 改行なしなら `estimated_tokens` は「文字数 / 3」の切り上げ。
            Ok("x".repeat(size * 3))
        };
        let mut result = result_with(2, 6);
        apply_refs_limits(&mut result, budget(200), render).unwrap();
        assert_eq!(result.references.len(), 2, "参照は削らない");
        assert!(result.result_summary.is_none());
        let skipped = result.skipped.as_ref().unwrap();
        assert_eq!(skipped.paths.len(), 3);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
    }

    /// 申告に代表パスが無ければ、描画の回数は参照の縮小 ([`apply_limits`]) だけのときと同じ。
    #[test]
    fn no_extra_render_without_samples() {
        let original = result_with(150, 0);
        let limits = budget(400);
        let mut calls = 0;
        let mut result = original.clone();
        apply_refs_limits(&mut result, limits, |probe| {
            calls += 1;
            json(probe)
        })
        .unwrap();

        let mut baseline = 0;
        let references = original.references.clone();
        apply_limits(&references, limits, true, |k, summary| {
            baseline += 1;
            json(&RefsResult {
                references: references[..k].to_vec(),
                result_summary: summary.cloned(),
                ..original.clone()
            })
        })
        .unwrap();
        assert_eq!(calls, baseline);
    }

    /// 既知の制約 (既存の割り切り): 参照が 0 件で、代表パスを 0 件にしても申告の本文だけで
    /// 予算を超える場合、参照の省略が無いので `result_summary` (と `budget_exceeded`) は
    /// 出ない。代表パスを縮めたことは `examples_limited_by` で分かる。
    #[test]
    fn unmeetable_budget_without_reference_omission_has_no_summary() {
        let mut original = result_with(0, 0);
        original.truncations = unanalyzable_declarations();
        let limit = crate::models::result_summary::MIN_TOKEN_BUDGET;
        let mut result = original.clone();
        apply_refs_limits_json(&mut result, budget(limit)).unwrap();

        assert!(result.result_summary.is_none());
        assert!(tokens(&result) > limit, "この入力は予算に収まらない");
        for truncation in &result.truncations {
            let detail = truncation.detail.as_ref().unwrap();
            assert!(detail.examples.is_empty());
            assert_eq!(detail.count, 3);
            assert_eq!(detail.examples_limited_by, Some(TOKEN_BUDGET));
        }
    }

    /// バッチ: 呼び出し全体の 1 予算でも、件数上限までの参照を全部配ったまま代表パスを
    /// 縮める。申告が先頭以外の結果にあっても縮め、申告の無い結果はそのまま。
    #[test]
    fn batch_samples_shrink_before_any_slot_is_cut() {
        for carrier in [0usize, 2] {
            let mut batch: Vec<RefsResult> = ["Alpha", "Beta", "Gamma", "Delta"]
                .iter()
                .enumerate()
                .map(|(i, name)| RefsResult {
                    symbol: name.to_string(),
                    ..result_with(2, if i == carrier { 50 } else { 0 })
                })
                .collect();
            let refs_only: Vec<RefsResult> = batch
                .iter()
                .map(|r| RefsResult {
                    skipped: None,
                    ..r.clone()
                })
                .collect();
            let limit = estimated_tokens(&serde_json::to_string(&refs_only).unwrap()) + 300;
            let original = batch.clone();
            apply_refs_batch_limits_json(&mut batch, budget(limit)).unwrap();

            for (after, before) in batch.iter().zip(&original) {
                assert_eq!(refs_json(after), refs_json(before), "{}", after.symbol);
                assert!(after.result_summary.is_none(), "{}", after.symbol);
            }
            let skipped = batch[carrier].skipped.as_ref().unwrap();
            assert!(skipped.paths.len() < 50, "{}", skipped.paths.len());
            assert_eq!(skipped.generated, 60);
            assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
            for (i, result) in batch.iter().enumerate() {
                if i != carrier {
                    assert!(result.skipped.is_none(), "{}", result.symbol);
                }
            }
            let rendered = serde_json::to_string(&batch).unwrap();
            assert!(estimated_tokens(&rendered) <= limit);
        }
    }

    /// バッチ: 代表パスを 0 件にしても収まらないときだけスロットを削り、round-robin の
    /// 配分 (少ない名前を飢餓させない) は保つ。
    #[test]
    fn batch_slots_are_cut_only_after_samples_reach_zero() {
        let mut batch = vec![
            RefsResult {
                symbol: "hot".into(),
                ..result_with(100, 50)
            },
            RefsResult {
                symbol: "rare".into(),
                ..result_with(2, 0)
            },
        ];
        let refs_only = vec![result_with(100, 0), result_with(2, 0)];
        let limit = estimated_tokens(&serde_json::to_string(&refs_only).unwrap()) / 2;
        apply_refs_batch_limits_json(&mut batch, budget(limit)).unwrap();

        let skipped = batch[0].skipped.as_ref().unwrap();
        assert!(skipped.paths.is_empty());
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert!(batch[0].references.len() < 100);
        assert_eq!(batch[1].references.len(), 2, "少ない名前は飢餓しない");
        assert!(estimated_tokens(&serde_json::to_string(&batch).unwrap()) <= limit);
    }

    /// 採寸の目安に使う予算の値。サマリの `limits` に載る数字の桁だけが描画の大きさに効く。
    const PROBE_BUDGET: usize = 1_000;

    /// 件数上限 `max_results` で切った参照とそのサマリに、`cap` 件に縮めた申告を付けた
    /// 単一の結果の描画の大きさ。
    fn single_cost(original: &RefsResult, max_results: usize, cap: usize) -> usize {
        let limits = ResultLimits {
            max_results: Some(max_results),
            token_budget: Some(PROBE_BUDGET),
        };
        let shown = count_capped(original.references.len(), limits);
        let (skipped, truncations) = original.declarations_with_sample_cap(cap, TOKEN_BUDGET);
        tokens(&RefsResult {
            references: original.references[..shown].to_vec(),
            skipped,
            truncations,
            result_summary: build_summary(
                &original.references,
                shown,
                limits,
                original.input_is_complete(),
                shown,
                limits.token_budget,
            ),
            ..original.clone()
        })
    }

    /// [`single_cost`] のバッチ版 (件数上限だけを round-robin で配った状態)。
    fn batch_cost(batch: &[RefsResult], max_results: usize, cap: usize) -> usize {
        let limits = ResultLimits {
            max_results: Some(max_results),
            token_budget: Some(PROBE_BUDGET),
        };
        let complete_input = batch.iter().all(RefsResult::input_is_complete);
        let groups: Vec<&[SymbolReference]> =
            batch.iter().map(|r| r.references.as_slice()).collect();
        let plan = GroupedCountPlan::new(&groups, limits);
        let summaries = plan.summaries(&groups, &plan.count_only, limits, complete_input);
        let probes: Vec<RefsResult> = batch
            .iter()
            .zip(summaries)
            .zip(&plan.count_only)
            .map(|((r, result_summary), &shown)| {
                let (skipped, truncations) = r.declarations_with_sample_cap(cap, TOKEN_BUDGET);
                RefsResult {
                    references: r.references[..shown].to_vec(),
                    skipped,
                    truncations,
                    result_summary,
                    ..r.clone()
                }
            })
            .collect();
        estimated_tokens(&serde_json::to_string(&probes).unwrap())
    }

    /// 件数上限で参照が切れる場合も、件数上限のサマリ込みで採寸して代表パスだけを縮める。
    /// 参照は件数上限まで出し、`limited_by` は `max_results` だけのまま。
    #[test]
    fn samples_shrink_together_with_the_count_cap() {
        let original = result_with(4, 50);
        // 代表パスを 0 件にすれば収まり、50 件のままでは収まらない予算。
        let limit = (single_cost(&original, 2, 0) + single_cost(&original, 2, 50)) / 2;
        let limits = ResultLimits {
            max_results: Some(2),
            token_budget: Some(limit),
        };
        let mut result = original.clone();
        apply_refs_limits_json(&mut result, limits).unwrap();

        assert_eq!(
            refs_json(&result),
            serde_json::to_value(&original.references[..2]).unwrap()
        );
        let summary = result.result_summary.as_ref().unwrap();
        assert_eq!((summary.shown, summary.total), (2, 4));
        assert_eq!(summary.limited_by, vec![ResultLimitKind::MaxResults]);
        assert!(!summary.complete_input, "生成物を除外しているので不完全");
        assert!(!summary.budget_exceeded);
        let skipped = result.skipped.as_ref().unwrap();
        assert!(
            !skipped.paths.is_empty() && skipped.paths.len() < 50,
            "{}",
            skipped.paths.len()
        );
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert!(tokens(&result) <= limit, "{} > {limit}", tokens(&result));
    }

    /// `max_results = 0` でも省略のサマリは保ち、代表パスだけを縮める。縮められない部分
    /// だけで予算を超えるなら、代表パスを 0 件にしたうえで `budget_exceeded` を申告する。
    #[test]
    fn zero_max_results_keeps_the_summary_and_shrinks_samples() {
        let original = result_with(3, 50);
        let limit = (single_cost(&original, 0, 0) + single_cost(&original, 0, 50)) / 2;
        let mut result = original.clone();
        apply_refs_limits_json(
            &mut result,
            ResultLimits {
                max_results: Some(0),
                token_budget: Some(limit),
            },
        )
        .unwrap();
        assert!(result.references.is_empty());
        let summary = result.result_summary.as_ref().unwrap();
        assert_eq!((summary.shown, summary.omitted), (0, 3));
        assert_eq!(summary.limited_by, vec![ResultLimitKind::MaxResults]);
        assert!(!summary.budget_exceeded);
        let skipped = result.skipped.as_ref().unwrap();
        assert!(!skipped.paths.is_empty() && skipped.paths.len() < 50);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert!(tokens(&result) <= limit);

        // 解析できないソースの申告 10 件の本文だけで最小の予算を超える場合。
        let mut heavy = result_with(3, 50);
        heavy.truncations = unanalyzable_declarations();
        let limit = crate::models::result_summary::MIN_TOKEN_BUDGET;
        assert!(
            single_cost(&heavy, 0, 0) > limit,
            "前提: 0 件でも収まらない"
        );
        apply_refs_limits_json(
            &mut heavy,
            ResultLimits {
                max_results: Some(0),
                token_budget: Some(limit),
            },
        )
        .unwrap();
        let summary = heavy.result_summary.as_ref().unwrap();
        assert!(summary.budget_exceeded, "守れなかった予算は申告する");
        assert!(heavy.skipped.as_ref().unwrap().paths.is_empty());
        assert!(
            heavy
                .truncations
                .iter()
                .all(|t| t.detail.as_ref().unwrap().examples.is_empty())
        );
    }

    /// バッチで件数上限による配分 (`[4, 2]` を 3 件 → `[2, 1]`) と代表パスの縮小が同時に
    /// 起きる。申告は後ろの結果にあり、サマリは件数上限の分だけを申告する。
    #[test]
    fn batch_samples_shrink_together_with_the_count_cap() {
        let original = vec![
            RefsResult {
                symbol: "four".into(),
                ..result_with(4, 0)
            },
            RefsResult {
                symbol: "two".into(),
                ..result_with(2, 50)
            },
        ];
        let limit = (batch_cost(&original, 3, 0) + batch_cost(&original, 3, 50)) / 2;
        let mut batch = original.clone();
        apply_refs_batch_limits_json(
            &mut batch,
            ResultLimits {
                max_results: Some(3),
                token_budget: Some(limit),
            },
        )
        .unwrap();

        let shown: Vec<usize> = batch.iter().map(|r| r.references.len()).collect();
        assert_eq!(shown, vec![2, 1], "件数上限の round-robin 配分のまま");
        for (result, total) in batch.iter().zip([4, 2]) {
            let summary = result.result_summary.as_ref().unwrap();
            assert_eq!(summary.total, total, "{}", result.symbol);
            assert_eq!(summary.limited_by, vec![ResultLimitKind::MaxResults]);
            assert!(!summary.complete_input, "{}", result.symbol);
            assert!(!summary.budget_exceeded, "{}", result.symbol);
        }
        assert!(batch[0].skipped.is_none());
        let skipped = batch[1].skipped.as_ref().unwrap();
        assert!(!skipped.paths.is_empty() && skipped.paths.len() < 50);
        assert_eq!(skipped.paths_limited_by, Some(TOKEN_BUDGET));
        assert!(estimated_tokens(&serde_json::to_string(&batch).unwrap()) <= limit);
    }
}
