//! 共変更コミットの可換集計とランキング。
//!
//! Git から証拠を取得する処理とは独立した純粋な集計層。rayon の reduce 順序に依存しない
//! ヒストグラムを保持し、最終段階で決定的に浮動小数点スコアへ変換する。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::models::cochange::{CoChangeEntry, CoChangeOptions};

#[derive(Default)]
pub(super) struct ShardStats {
    pub(super) per_source_raw: Vec<HashMap<String, usize>>,
    pub(super) per_source_weight_hist: Vec<HashMap<String, BTreeMap<usize, u64>>>,
    pub(super) counted_denom: Vec<usize>,
    pub(super) denom_weight_hist: Vec<BTreeMap<usize, u64>>,
    pub(super) denom_units: Vec<HashSet<(String, i64)>>,
    pub(super) co_units: Vec<HashMap<String, HashSet<(String, i64)>>>,
    pub(super) co_counts: HashMap<String, usize>,
    pub(super) failed_commits: usize,
}

impl ShardStats {
    pub(super) fn new(n_sources: usize) -> Self {
        Self {
            per_source_raw: (0..n_sources).map(|_| HashMap::new()).collect(),
            per_source_weight_hist: (0..n_sources).map(|_| HashMap::new()).collect(),
            counted_denom: vec![0; n_sources],
            denom_weight_hist: (0..n_sources).map(|_| BTreeMap::new()).collect(),
            denom_units: (0..n_sources).map(|_| HashSet::new()).collect(),
            co_units: (0..n_sources).map(|_| HashMap::new()).collect(),
            co_counts: HashMap::new(),
            failed_commits: 0,
        }
    }

    /// 可換な整数度数だけを merge する。浮動小数点加算はここでは行わない。
    pub(super) fn merge(mut a: Self, b: Self) -> Self {
        if a.denom_weight_hist.is_empty() {
            let mut b = b;
            b.failed_commits += a.failed_commits;
            return b;
        }
        if b.denom_weight_hist.is_empty() {
            a.failed_commits += b.failed_commits;
            return a;
        }
        for (i, m) in b.per_source_raw.into_iter().enumerate() {
            for (k, v) in m {
                *a.per_source_raw[i].entry(k).or_insert(0) += v;
            }
        }
        for (i, c) in b.counted_denom.into_iter().enumerate() {
            a.counted_denom[i] += c;
        }
        for (i, m) in b.per_source_weight_hist.into_iter().enumerate() {
            for (k, hist) in m {
                let dst = a.per_source_weight_hist[i].entry(k).or_default();
                for (file_count, n) in hist {
                    *dst.entry(file_count).or_insert(0) += n;
                }
            }
        }
        for (i, hist) in b.denom_weight_hist.into_iter().enumerate() {
            for (file_count, n) in hist {
                *a.denom_weight_hist[i].entry(file_count).or_insert(0) += n;
            }
        }
        for (i, set) in b.denom_units.into_iter().enumerate() {
            a.denom_units[i].extend(set);
        }
        for (i, map) in b.co_units.into_iter().enumerate() {
            for (k, set) in map {
                a.co_units[i].entry(k).or_default().extend(set);
            }
        }
        for (k, v) in b.co_counts {
            *a.co_counts.entry(k).or_insert(0) += v;
        }
        a.failed_commits += b.failed_commits;
        a
    }

    pub(super) fn smoothed_score(
        &self,
        i: usize,
        cand: &str,
        opts: &CoChangeOptions,
        author_unit_active: bool,
    ) -> f64 {
        let alpha = opts.smoothing_alpha;
        let beta = opts.smoothing_beta;
        if author_unit_active && !self.denom_units[i].is_empty() {
            let denom_units_n = self.denom_units[i].len() as f64;
            let co_units_n = self.co_units[i].get(cand).map(|s| s.len()).unwrap_or(0) as f64;
            (co_units_n + alpha) / (denom_units_n + alpha + beta)
        } else {
            let sum_weights = |hist: &BTreeMap<usize, u64>| -> f64 {
                hist.iter()
                    .map(|(&file_count, &n)| {
                        n as f64
                            * commit_size_weight(
                                file_count,
                                opts.commit_size_pivot,
                                opts.max_files_per_commit,
                            )
                    })
                    .sum()
            };
            let weighted_co = self.per_source_weight_hist[i]
                .get(cand)
                .map(sum_weights)
                .unwrap_or(0.0);
            let weighted_denom = sum_weights(&self.denom_weight_hist[i]);
            (weighted_co + alpha) / (weighted_denom + alpha + beta)
        }
    }
}

pub(super) fn commit_size_weight(file_count: usize, pivot: usize, hard_max: usize) -> f64 {
    if hard_max > 0 && file_count > hard_max {
        return 0.0;
    }
    if pivot == 0 {
        return 1.0;
    }
    let n = file_count.max(1) as f64;
    let p = pivot as f64;
    (p / n).sqrt().min(1.0)
}

pub(super) fn compare_entries_by_ranking(
    a: &CoChangeEntry,
    b: &CoChangeEntry,
    smoothing_on: bool,
) -> std::cmp::Ordering {
    b.ranking_value(smoothing_on)
        .partial_cmp(&a.ranking_value(smoothing_on))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| b.co_changes.cmp(&a.co_changes))
        .then_with(|| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| a.is_history_evidence().cmp(&b.is_history_evidence()))
        .then_with(|| a.file_a.cmp(&b.file_a))
        .then_with(|| a.file_b.cmp(&b.file_b))
}
