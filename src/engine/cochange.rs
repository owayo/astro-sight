use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{
    BLAME_DEFAULT_EXCLUDE_GLOBS, CoChangeDiagnosticReason, CoChangeDiagnostics, CoChangeEntry,
    CoChangeEvidence, CoChangeOptions, CoChangeResult,
};
use crate::models::skip::SkipInfo;

/// `git diff` / `git blame` 等に渡す revision を検証する。
/// 先頭が `-` の値はオプションとして解釈されるため拒否する
/// (`--output=/path` のようなファイル書き込みオプション混入を防ぐ)。
fn validate_revision(rev: &str, arg_name: &str) -> Result<()> {
    if rev.is_empty() {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not be empty"),
        ));
    }
    if rev.starts_with('-') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not start with '-': {rev}"),
        ));
    }
    if rev.contains('\0') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not contain NUL"),
        ));
    }
    Ok(())
}

/// entries なし・commits_analyzed 0 の空結果を返す。起点なし / 証拠集合が空 /
/// マージ除外後に空、の各早期 return で共用し、構築の重複を避ける。
fn empty_cochange_result(
    skipped: Option<SkipInfo>,
    mut diagnostics: CoChangeDiagnostics,
) -> CoChangeResult {
    diagnostics.finalize();
    CoChangeResult {
        entries: Vec::new(),
        commits_analyzed: 0,
        diagnostics,
        skipped,
    }
}

/// blame ベースの共変更解析。
///
/// 起点ファイル `source_files` の **変更行** に対して `git blame -L` を当て、
/// 最終修正コミット集合 `C` を作る。各 c ∈ C の `git diff-tree --name-only -r c`
/// から起点以外の共起ファイルを集計し、`co_changes / |C|` を confidence とする。
///
/// 起点ファイルに密結合なペアだけが浮上するため、lookback 系より文脈依存性が高い。
/// 大規模リポでも履歴全体を舐めずに済む。
///
/// 変更行 blame で `min_denominator` 分の証拠が作れない起点 (diff が無い / 純粋追加
/// hunk のみ / 変更行が 1 コミットにしか遡れない) は、`history_limit > 0` のとき
/// `git log <base> -n <limit> -- <file>` のファイル履歴を代替証拠にフォールバックする。
/// base 側に存在しない新規追加ファイルは履歴が無いため起点にならない (診断に記録する)。
pub fn analyze_cochange(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    if opts.source_files.is_empty() {
        return Ok(empty_cochange_result(None, CoChangeDiagnostics::default()));
    }
    // 起点ファイル数の上限ガード (0 = 無制限)。暴走防止のため超過は明示的に停止する。
    if opts.max_source_files > 0 && opts.source_files.len() > opts.max_source_files {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "source_files count {} exceeds --max-source-files limit {}; \
                 narrow --paths or raise the limit explicitly",
                opts.source_files.len(),
                opts.max_source_files,
            ),
        ));
    }

    // 全体タイムアウト用のチェックポイント (0 = 無制限)。
    // 各 Phase 入口で elapsed を確認し、超過なら InvalidRequest で停止する。
    // 既に走行中の subprocess (git blame / diff-tree) は kill しないため、
    // 直近の 1 起動の完了までは待つ実装 (実用上の許容範囲)。
    let started = std::time::Instant::now();
    let check_timeout = |phase: &str| -> Result<()> {
        if opts.timeout_secs == 0 {
            return Ok(());
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= opts.timeout_secs {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "blame analysis exceeded --timeout-secs {} during {phase} (elapsed {elapsed}s)",
                    opts.timeout_secs,
                ),
            ));
        }
        Ok(())
    };

    let base_rev: &str = opts
        .base
        .as_deref()
        .unwrap_or(crate::commands::DEFAULT_BLAME_BASE);
    // base は git diff / git blame に直接渡されるため、`-` プレフィクス等の
    // オプション誤認識を防ぐ revision 検証を必ず通す。--paths/--paths-file 経由で
    // resolve_blame_source_files の検証を迂回しても安全側に倒れる。
    validate_revision(base_rev, "--base")?;

    let mut diag = CoChangeDiagnostics {
        sources_requested: opts.source_files.len(),
        ..Default::default()
    };
    let min_denom = opts.min_denominator.max(1);

    // Phase 1: 起点ファイルごとに証拠コミット集合を作る。
    //          第一選択は変更行 blame、証拠が min_denominator に満たなければ履歴 fallback。
    //          ファイル単位で rayon 並列化する。
    check_timeout("phase1_blame_setup")?;
    let evidence_per_file: Vec<SourceEvidence> = opts
        .source_files
        .par_iter()
        .map(|f| collect_evidence_for_file(dir, f, base_rev, min_denom, opts))
        .collect();
    check_timeout("phase1_blame_collected")?;

    for ev in &evidence_per_file {
        if ev.git_failed {
            diag.add_reason(CoChangeDiagnosticReason::GitCommandFailed);
        }
        if ev.no_changed_old_lines {
            diag.add_reason(CoChangeDiagnosticReason::NoChangedOldLines);
        }
        if ev.commits.is_empty() {
            diag.sources_without_evidence += 1;
            if ev.new_file {
                diag.new_files_without_history += 1;
                diag.add_reason(CoChangeDiagnosticReason::NewFileHasNoPriorHistory);
            } else {
                diag.add_reason(CoChangeDiagnosticReason::NoCommitEvidence);
            }
            continue;
        }
        match ev.mode {
            CoChangeEvidence::Blame => diag.sources_with_blame_evidence += 1,
            CoChangeEvidence::History => diag.sources_with_history_evidence += 1,
        }
    }

    let mut commit_set: HashSet<String> = HashSet::new();
    for ev in &evidence_per_file {
        for k in ev.commits.keys() {
            commit_set.insert(k.clone());
        }
    }
    if commit_set.is_empty() {
        return Ok(empty_cochange_result(None, diag));
    }

    // SHA 集合の上限ガード (0 = 無制限)。
    // 病理的に巨大な blame 集合 (数万規模) で続く diff-tree 並列爆発を防ぐ防衛線。
    if opts.max_blame_commits > 0 && commit_set.len() > opts.max_blame_commits {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "blame commit set size {} exceeds --max-blame-commits limit {}; \
                 narrow --paths/--base or raise the limit explicitly",
                commit_set.len(),
                opts.max_blame_commits,
            ),
        ));
    }

    // 必要に応じてマージコミットを除外する。
    // git rev-list --no-walk --merges <SHA>... は引数の SHA のうちマージのみを返すので、
    // 一括問い合わせで効率良く判定できる (ARG_MAX 超過対策で chunk 化)。
    if opts.ignore_merges {
        check_timeout("phase2_merge_filter")?;
        let merge_set = list_merge_commits(dir, &commit_set)?;
        commit_set.retain(|s| !merge_set.contains(s));
        if commit_set.is_empty() {
            return Ok(empty_cochange_result(None, diag));
        }
    }
    // 起点別の証拠集合からも同 SHA を除外して per-source 集計と整合させる。
    let evidence_per_file: Vec<SourceEvidence> = if opts.ignore_merges {
        evidence_per_file
            .into_iter()
            .map(|mut ev| {
                ev.commits.retain(|sha, _| commit_set.contains(sha));
                ev
            })
            .collect()
    } else {
        evidence_per_file
    };
    check_timeout("phase3_diff_tree_setup")?;

    // Phase 2: SHA → 起点ファイル indices の逆引きを作り、SHA → BlameInfo もマージする。
    // diff-tree 結果を全保持せず、各 SHA を 1-pass で per-source 集計に畳み込むため、
    // 「この SHA にヒットする起点ファイルは誰か」を引けるようにしておく。
    let mut sha_to_sources: HashMap<String, Vec<usize>> = HashMap::new();
    let mut sha_to_info: HashMap<String, BlameInfo> = HashMap::new();
    for (i, ev) in evidence_per_file.iter().enumerate() {
        for (sha, info) in &ev.commits {
            sha_to_sources.entry(sha.clone()).or_default().push(i);
            // 起点間で同じ SHA の info があれば最初に見たものを採用する
            sha_to_info
                .entry(sha.clone())
                .or_insert_with(|| info.clone());
        }
    }
    let denominator = sha_to_sources.len();
    let n_sources = opts.source_files.len();
    let source_set: HashSet<&str> = opts.source_files.iter().map(String::as_str).collect();
    let exclude_matcher = build_exclude_matcher(&opts.exclude_globs)?;

    // author_unit_window_days > 0 のとき、各 SHA を「(author_mail, time_bucket)」の unit に
    // 圧縮することで、同一 author の連続 commit を 1 knowledge unit として扱う。
    // window=0 (旧挙動) のとき unit_key は常に None で、raw weighted 集計のみが有効。
    let author_window_days = opts.author_unit_window_days;
    let unit_key_for = |sha: &str| -> Option<(String, i64)> {
        if author_window_days == 0 {
            return None;
        }
        let info = sha_to_info.get(sha)?;
        let mail = info.author_mail.as_ref()?;
        let time = info.author_time?;
        let bucket_seconds = (author_window_days as i64) * 86_400;
        Some((mail.clone(), time / bucket_seconds))
    };

    // Phase 3 (streaming): 各 SHA について diff-tree 取得 → サイズ重み計算 →
    // per-source 集計 (raw / weighted / units) と global co_counts を rayon の fold/reduce で
    // 直接畳み込む。`commit_files: Vec<Vec<String>>` を全保持しないため、
    // 大規模 diff (commits 数百〜千超) でもピーク RSS が線形以下に収まる。
    let sha_entries: Vec<(String, Vec<usize>)> = sha_to_sources.into_iter().collect();

    let stats: ShardStats = sha_entries
        .par_iter()
        .fold(
            || ShardStats::new(n_sources),
            |mut acc, (sha, sources)| {
                let files = collect_files_in_commit(dir, sha).unwrap_or_default();
                let weight = commit_size_weight(
                    files.len(),
                    opts.commit_size_pivot,
                    opts.max_files_per_commit,
                );
                if weight <= 0.0 {
                    return acc;
                }
                let unit = unit_key_for(sha);
                for &i in sources {
                    // confidence の分母は「実際に集計対象となったコミット数」。
                    // weight 0 (max_files_per_commit 超過) のコミットは上で return 済みなので
                    // ここに到達したものだけを数える。証拠集合サイズをそのまま分母にすると
                    // co (超過コミットを含まない) と母数がずれ confidence が過小評価になる。
                    acc.counted_denom[i] += 1;
                    acc.weighted_denom[i] += weight;
                    if let Some(u) = unit.as_ref() {
                        acc.denom_units[i].insert(u.clone());
                    }
                }
                let mut seen: HashSet<&str> = HashSet::new();
                for f in &files {
                    let path = f.as_str();
                    if !seen.insert(path) {
                        continue;
                    }
                    if source_set.contains(path) {
                        continue;
                    }
                    if exclude_matcher.is_match(path) {
                        continue;
                    }
                    *acc.co_counts.entry(path.to_string()).or_insert(0) += 1;
                    for &i in sources {
                        *acc.per_source_raw[i].entry(path.to_string()).or_insert(0) += 1;
                        *acc.per_source_weighted[i]
                            .entry(path.to_string())
                            .or_insert(0.0) += weight;
                        if let Some(u) = unit.as_ref() {
                            acc.co_units[i]
                                .entry(path.to_string())
                                .or_default()
                                .insert(u.clone());
                        }
                    }
                }
                acc
            },
        )
        .reduce(|| ShardStats::new(n_sources), ShardStats::merge);

    check_timeout("phase4_assemble")?;

    // Phase 4: 各起点に対して CoChangeEntry を構築する。stats は streaming で
    // すでに per-source 集計済みなので、ここでは閾値フィルタとランキングのみ行う。
    //
    // 閾値は raw `confidence` (と、明示指定時のみ `min_score`) に対して適用する。
    // 平滑化した `score` はランキング専用。score は分母が小さいほど 0 に引き寄せられる
    // shrinkage 推定値なので、閾値と突き合わせると「変更行 blame の分母が小さい起点は
    // 100% 共変更でも構造的に出力されない」(既定 β=8 では分母 2 で上限 0.27 < 0.3) 。
    let smoothing_on = !opts.disable_smoothing;
    let alpha = opts.smoothing_alpha;
    let beta = opts.smoothing_beta;
    let author_unit_active = author_window_days > 0;
    let score_gate_on = opts.min_score > 0.0;

    let mut entries: Vec<CoChangeEntry> = Vec::new();
    for (i, source) in opts.source_files.iter().enumerate() {
        let evidence_set_len = evidence_per_file[i].commits.len();
        let counted_denom = stats.counted_denom[i];
        if counted_denom < min_denom {
            if evidence_set_len > 0 {
                diag.skipped_min_denominator += 1;
                diag.add_reason(CoChangeDiagnosticReason::BelowMinDenominator);
            }
            continue;
        }
        let local_denom_raw = counted_denom as f64;
        let weighted_denom = stats.weighted_denom[i];
        let raw = &stats.per_source_raw[i];
        let weighted = &stats.per_source_weighted[i];
        // blame 由来か履歴 fallback 由来かをエントリに引き継ぐ (既定 Blame は省略)。
        let evidence = match evidence_per_file[i].mode {
            CoChangeEvidence::Blame => None,
            CoChangeEvidence::History => Some(CoChangeEvidence::History),
        };

        let mut per_source_entries: Vec<CoChangeEntry> = Vec::new();
        for (cand, co) in raw {
            let co = *co;
            diag.candidate_pairs += 1;
            if co < opts.min_samples {
                diag.filtered_min_samples += 1;
                diag.add_reason(CoChangeDiagnosticReason::BelowMinSamples);
                continue;
            }
            let confidence = co as f64 / local_denom_raw;
            if confidence < opts.min_confidence {
                diag.filtered_min_confidence += 1;
                diag.add_reason(CoChangeDiagnosticReason::BelowMinConfidence);
                continue;
            }
            // author_unit_window_days > 0 のとき、score は unit ベース
            // (|co_units| + α) / (|denom_units| + α + β) で計算する。
            // unit が空 (author 情報を取れなかった SHA のみ) のとき raw weighted にフォールバック。
            let score = if smoothing_on {
                if author_unit_active && !stats.denom_units[i].is_empty() {
                    let denom_units_n = stats.denom_units[i].len() as f64;
                    let co_units_n =
                        stats.co_units[i].get(cand).map(|s| s.len()).unwrap_or(0) as f64;
                    (co_units_n + alpha) / (denom_units_n + alpha + beta)
                } else {
                    let weighted_co = *weighted.get(cand).unwrap_or(&0.0);
                    (weighted_co + alpha) / (weighted_denom + alpha + beta)
                }
            } else {
                confidence
            };
            if score_gate_on && score < opts.min_score {
                diag.filtered_min_score += 1;
                diag.add_reason(CoChangeDiagnosticReason::BelowMinScore);
                continue;
            }
            per_source_entries.push(CoChangeEntry {
                file_a: source.clone(),
                file_b: cand.clone(),
                co_changes: co,
                total_changes_a: evidence_set_len,
                total_changes_b: *stats.co_counts.get(cand).unwrap_or(&0),
                confidence,
                denominator: Some(counted_denom),
                score: Some(score),
                evidence,
            });
        }
        per_source_entries.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));
        if opts.per_source_limit > 0 && per_source_entries.len() > opts.per_source_limit {
            diag.truncated_per_source_limit += per_source_entries.len() - opts.per_source_limit;
            per_source_entries.truncate(opts.per_source_limit);
        }
        entries.extend(per_source_entries);
    }

    // 全体 ranking。smoothing 有効なら score 降順、無効なら confidence 降順。
    entries.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));
    diag.finalize();

    Ok(CoChangeResult {
        entries,
        commits_analyzed: denominator,
        diagnostics: diag,
        skipped: None,
    })
}

/// streaming 集計用のスレッドローカル統計。fold/reduce でマージされる。
/// - `per_source_raw` / `per_source_weighted` / `weighted_denom`: commit-size weighting の集計
/// - `counted_denom`: 実際に集計対象となったコミット数 (= confidence の分母)
/// - `denom_units` / `co_units`: author_unit_window_days > 0 のとき (author, time_bucket) 単位で
///   起点ファイル別の unique unit と候補別の unique unit を保持する
/// - `co_counts`: グローバル候補出現数 (= `CoChangeEntry.total_changes_b` の元)
#[derive(Default)]
struct ShardStats {
    per_source_raw: Vec<HashMap<String, usize>>,
    per_source_weighted: Vec<HashMap<String, f64>>,
    counted_denom: Vec<usize>,
    weighted_denom: Vec<f64>,
    denom_units: Vec<HashSet<(String, i64)>>,
    co_units: Vec<HashMap<String, HashSet<(String, i64)>>>,
    co_counts: HashMap<String, usize>,
}

impl ShardStats {
    fn new(n_sources: usize) -> Self {
        Self {
            per_source_raw: (0..n_sources).map(|_| HashMap::new()).collect(),
            per_source_weighted: (0..n_sources).map(|_| HashMap::new()).collect(),
            counted_denom: vec![0; n_sources],
            weighted_denom: vec![0.0; n_sources],
            denom_units: (0..n_sources).map(|_| HashSet::new()).collect(),
            co_units: (0..n_sources).map(|_| HashMap::new()).collect(),
            co_counts: HashMap::new(),
        }
    }

    fn merge(mut a: Self, b: Self) -> Self {
        if a.weighted_denom.is_empty() {
            return b;
        }
        if b.weighted_denom.is_empty() {
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
        for (i, m) in b.per_source_weighted.into_iter().enumerate() {
            for (k, v) in m {
                *a.per_source_weighted[i].entry(k).or_insert(0.0) += v;
            }
        }
        for (i, w) in b.weighted_denom.into_iter().enumerate() {
            a.weighted_denom[i] += w;
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
        a
    }
}

/// 1 コミットあたりの「サイズ重み」を返す。
/// - `pivot=0`: 常に 1.0 (旧挙動、size weighting 無効)
/// - `pivot>0`: `min(1.0, sqrt(pivot/file_count))`
///   小コミット (file_count <= pivot) は 1.0、大コミットほど 0 に近づく
/// - `hard_max>0` かつ `file_count > hard_max`: 0.0 (スキップ済み)
fn commit_size_weight(file_count: usize, pivot: usize, hard_max: usize) -> f64 {
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

/// CoChangeEntry を ranking 値で降順比較する。
/// 同値時は co_changes (実共起数) 降順 → confidence 降順 → blame 証拠優先 →
/// path 昇順の順で安定化する。
/// ranking value (score / confidence) が同点でも、より多くのコミットで一緒に変更された
/// ペアを優先することで、低 score 帯でのランキング品質を体感的に改善する。
/// blame 証拠 (起点の変更行に直結) は履歴 fallback 証拠より密結合なので同点なら先に出す。
fn compare_entries_by_ranking(
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

/// 起点 1 ファイル分のコミット証拠。
///
/// `mode` が `Blame` なら起点の**変更行**を最後に触ったコミット集合、`History` なら
/// ファイル自身の履歴。`commits` が空なら証拠を作れなかったことを意味し、`new_file` /
/// `no_changed_old_lines` / `git_failed` がその理由を伝える (診断出力に使う)。
struct SourceEvidence {
    commits: HashMap<String, BlameInfo>,
    mode: CoChangeEvidence,
    /// blame 可能な変更行が 1 行も無かった (diff が空 / 純粋追加 hunk のみ)。
    no_changed_old_lines: bool,
    /// base..HEAD で新規追加されたファイル (base 側に履歴が無い)。
    new_file: bool,
    /// git コマンドが失敗した (証拠なしと区別するため保持する)。
    git_failed: bool,
}

impl SourceEvidence {
    fn empty(mode: CoChangeEvidence) -> Self {
        Self {
            commits: HashMap::new(),
            mode,
            no_changed_old_lines: false,
            new_file: false,
            git_failed: false,
        }
    }
}

/// 起点 1 ファイルの証拠コミット集合を作る。
///
/// 第一選択は変更行 blame。得られた集合が `min_denom` に満たない場合
/// (diff が無い / 純粋追加のみ / 変更行が 1 コミットにしか遡れない) は、
/// `history_limit > 0` ならファイル履歴へフォールバックする。
///
/// 履歴は `git log <base>` から取るため、base 側に存在しない新規追加ファイルでは
/// 空になる。新規ファイルの「追加コミット = 今レビュー中の diff そのもの」を証拠に
/// してしまうと、同一コミットの全ファイルが confidence 1.0 の共変更として並ぶだけで
/// 情報量がゼロになるため、これは意図した挙動。
fn collect_evidence_for_file(
    dir: &str,
    file: &str,
    base: &str,
    min_denom: usize,
    opts: &CoChangeOptions,
) -> SourceEvidence {
    let changed = collect_changed_old_ranges(dir, file, base).unwrap_or(ChangedOldRanges {
        git_failed: true,
        ..Default::default()
    });

    let mut ev = SourceEvidence::empty(CoChangeEvidence::Blame);
    ev.new_file = changed.new_file;
    ev.git_failed = changed.git_failed;
    ev.no_changed_old_lines = changed.ranges.is_empty();

    if !changed.ranges.is_empty() {
        match collect_blame_commits_for_file(
            dir,
            file,
            base,
            &changed.ranges,
            opts.rename,
            opts.copy,
        ) {
            Ok(commits) => ev.commits = commits,
            Err(_) => ev.git_failed = true,
        }
    }

    // blame 証拠が分母の下限に届かないときだけ履歴へ広げる。
    // blame で十分な証拠が取れている起点は、より密結合な blame 側を維持する。
    if ev.commits.len() >= min_denom || opts.history_limit == 0 {
        return ev;
    }
    match collect_history_commits_for_file(dir, file, base, opts) {
        Ok(history) => {
            if history.len() > ev.commits.len() {
                ev.commits = history;
                ev.mode = CoChangeEvidence::History;
            }
        }
        Err(_) => ev.git_failed = true,
    }
    ev
}

/// `collect_changed_old_ranges` の結果。同型の bool を位置引数で返すと
/// 取り違えてもコンパイルが通るため構造体で束ねる。
#[derive(Default)]
struct ChangedOldRanges {
    /// blame 対象にする hunk 旧側 (start, count)。純粋追加 hunk は含まない。
    ranges: Vec<(u64, u64)>,
    /// base 側にファイルが無い (= base..HEAD で新規追加された)。
    new_file: bool,
    /// git コマンドが失敗した。
    git_failed: bool,
}

/// `git diff --unified=0 <base> HEAD -- <file>` から hunk 旧側 range を抽出する。
///
/// `new_file` は diff ヘッダの `--- /dev/null` で判定する
/// (base 側にファイルが無い = 新規追加 = 履歴 fallback も効かない)。
fn collect_changed_old_ranges(dir: &str, file: &str, base: &str) -> Result<ChangedOldRanges> {
    // revision は `<base>` 単独 (`<base> HEAD` ではない)。2 revision 形式では
    // 未コミットの作業ツリー変更が hunk に現れず、pre-commit レビュー時に
    // 変更行 blame の証拠が常に空になる (他の --git コマンドと意味を揃える)。
    let diff_output = Command::new("git")
        .args(["diff", "--unified=0", base, "--", file])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !diff_output.status.success() {
        return Ok(ChangedOldRanges {
            git_failed: true,
            ..Default::default()
        });
    }
    let diff_text = String::from_utf8_lossy(&diff_output.stdout);
    Ok(ChangedOldRanges {
        ranges: parse_hunk_old_ranges(&diff_text),
        new_file: diff_text.lines().any(|l| l == "--- /dev/null"),
        git_failed: false,
    })
}

/// 1 ファイルの diff hunk 群を 1 回の `git blame -L S,+C [-L S,+C]...` にまとめて
/// 最終修正コミット SHA 集合を取得する。
/// - 純粋追加 hunk (old_count = 0) は呼び出し側の `parse_hunk_old_ranges` で除外済み。
/// - blame の失敗 (binary / non-existent in base) は空集合を返して継続。
///
/// 戻り値は SHA → BlameInfo (`author_mail` / `author_time`) の map。
/// author_unit_window_days > 0 のとき、unit ベース集計のキーとして使われる。
/// porcelain で取れない場合は None のままにする (window=0 と等価扱い)。
fn collect_blame_commits_for_file(
    dir: &str,
    file: &str,
    base: &str,
    ranges: &[(u64, u64)],
    rename: bool,
    copy: bool,
) -> Result<HashMap<String, BlameInfo>> {
    if ranges.is_empty() {
        return Ok(HashMap::new());
    }

    // 1 起動の git blame に複数 -L を渡す。
    // rename=true: `-M` でファイル内移動 + ファイル間 rename を追跡。
    // copy=true:   `-C` でファイル間コピーも検出 (`-M` より重い、別フラグでオプトイン)。
    let mut args: Vec<String> = vec!["blame".into(), "--line-porcelain".into()];
    if rename {
        args.push("-M".into());
    }
    if copy {
        args.push("-C".into());
    }
    for (start, count) in ranges {
        args.push("-L".into());
        args.push(format!("{},+{}", start, count));
    }
    args.push(base.to_string());
    args.push("--".into());
    args.push(file.to_string());

    let blame_output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !blame_output.status.success() {
        return Ok(HashMap::new());
    }
    let blame_text = String::from_utf8_lossy(&blame_output.stdout);

    parse_blame_porcelain(&blame_text)
}

/// blame `--line-porcelain` 出力から SHA → BlameInfo を抽出する。
/// 各 entry の先頭行 `<sha40> <orig> <final> [count]` をエントリ境界とし、
/// それ以降の `author-mail <addr>` / `author-time <unix>` を SHA に紐付ける。
fn parse_blame_porcelain(blame_text: &str) -> Result<HashMap<String, BlameInfo>> {
    let mut out: HashMap<String, BlameInfo> = HashMap::new();
    let mut current_sha: Option<String> = None;
    for line in blame_text.lines() {
        let bytes = line.as_bytes();
        // entry header: 40 hex + space + 残り
        if bytes.len() >= 41
            && bytes[40] == b' '
            && bytes[..40].iter().all(|b| b.is_ascii_hexdigit())
        {
            let sha = std::str::from_utf8(&bytes[..40])
                .expect("ascii hex is utf-8")
                .to_string();
            // 既出の SHA でも info を上書きしない (最初に出た情報を信頼)
            out.entry(sha.clone()).or_default();
            current_sha = Some(sha);
            continue;
        }
        // メタデータ行 (`author-mail <addr>` / `author-time <unix>`)
        let Some(sha) = current_sha.as_ref() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("author-mail ") {
            // git porcelain の author-mail は通常 `<addr>` で囲まれている
            let trimmed = rest.trim().trim_start_matches('<').trim_end_matches('>');
            if !trimmed.is_empty() {
                out.entry(sha.clone()).or_default().author_mail = Some(trimmed.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("author-time ")
            && let Ok(t) = rest.trim().parse::<i64>()
        {
            out.entry(sha.clone()).or_default().author_time = Some(t);
        }
    }
    Ok(out)
}

/// blame で抽出した SHA ごとのコミットメタデータ。
/// `author_unit_window_days > 0` のとき、(author_mail, time_bucket) を unit キーとして使う。
#[derive(Debug, Clone, Default)]
struct BlameInfo {
    author_mail: Option<String>,
    author_time: Option<i64>,
}

/// 履歴 fallback: `git log <base> -n <limit> -- <file>` でファイル自身のコミット集合を取る。
///
/// 変更行 blame が使えない起点 (diff が無い / 純粋追加のみ) や、blame 集合が小さすぎて
/// `min_denominator` を満たさない起点の代替証拠。`%H%x00%ae%x00%at` で SHA・author mail・
/// author time を 1 起動でまとめて取り、blame 経路と同じ `BlameInfo` を組み立てる。
///
/// - `<base>` を起点にするため、レビュー対象コミット自身は含まれない。
/// - `ignore_merges` のときは `--no-merges` を付ける。後段の一括マージ除外でも落ちるが、
///   ここで除外しておかないと limit 件がマージだけで埋まって実質 0 件になり得る。
/// - `rename` のときは `--follow` を付けてリネーム境界を越える。
/// - git の失敗 (base に存在しないパス等) は Err ではなく空集合を返して継続する。
fn collect_history_commits_for_file(
    dir: &str,
    file: &str,
    base: &str,
    opts: &CoChangeOptions,
) -> Result<HashMap<String, BlameInfo>> {
    if opts.history_limit == 0 {
        return Ok(HashMap::new());
    }
    let mut args: Vec<String> = vec![
        "log".into(),
        "--format=%H%x00%ae%x00%at".into(),
        format!("-n{}", opts.history_limit),
    ];
    if opts.ignore_merges {
        args.push("--no-merges".into());
    }
    if opts.rename {
        args.push("--follow".into());
    }
    args.push(base.to_string());
    args.push("--".into());
    args.push(file.to_string());

    let output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        return Ok(HashMap::new());
    }
    let mut out: HashMap<String, BlameInfo> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split('\0');
        let Some(sha) = parts.next() else { continue };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mail = parts.next().unwrap_or("").trim();
        let time = parts.next().unwrap_or("").trim();
        out.insert(
            sha.to_string(),
            BlameInfo {
                author_mail: (!mail.is_empty()).then(|| mail.to_string()),
                author_time: time.parse::<i64>().ok(),
            },
        );
    }
    Ok(out)
}

/// `@@ -OLDSTART[,OLDCOUNT] +NEWSTART[,NEWCOUNT] @@` の OLDSTART/OLDCOUNT を抽出する。
/// COUNT 省略時は 1。COUNT が 0 の hunk (純粋追加) は除外する。
fn parse_hunk_old_ranges(diff_text: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        let Some(rest) = line.strip_prefix("@@ -") else {
            continue;
        };
        // rest 例: "32,3 +33,7 @@ ..."
        let Some(end) = rest.find(' ') else {
            continue;
        };
        let token = &rest[..end];
        let (start_s, count_s) = match token.split_once(',') {
            Some((a, b)) => (a, b),
            None => (token, "1"),
        };
        let Ok(start) = start_s.parse::<u64>() else {
            continue;
        };
        let Ok(count) = count_s.parse::<u64>() else {
            continue;
        };
        if count == 0 {
            continue;
        }
        out.push((start, count));
    }
    out
}

/// `git rev-list --no-walk --merges <SHA>...` で指定 SHA のうちマージコミットだけを返す。
/// 引数長制限 (ARG_MAX) を避けるため、SHA は 256 件ごとにチャンク化して呼び出す。
/// SHA が 0 件の場合は空集合を返す。
fn list_merge_commits(dir: &str, shas: &HashSet<String>) -> Result<HashSet<String>> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    const CHUNK: usize = 256;
    let all: Vec<&String> = shas.iter().collect();
    let mut merges: HashSet<String> = HashSet::new();
    for chunk in all.chunks(CHUNK) {
        let mut args: Vec<String> = vec!["rev-list".into(), "--no-walk".into(), "--merges".into()];
        for s in chunk {
            args.push((*s).clone());
        }
        let output = Command::new("git")
            .args(&args)
            .current_dir(dir)
            .output()
            .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
        if !output.status.success() {
            // rev-list の失敗 (orphan SHA 等) は致命ではなく、マージ判定なしで続行する
            continue;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let s = line.trim();
            if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                merges.insert(s.to_string());
            }
        }
    }
    Ok(merges)
}

/// `git diff-tree --root --no-commit-id --name-only -r <sha>` でコミット c の変更ファイル一覧を返す。
/// `--root` を付けないとルート (初期) コミットは parent がないため空が返り、blame で
/// その SHA が拾われた場合に共変更が検出できない。`max_files_per_commit` で巨大な
/// 初期 import は除外されるので、`--root` を有効にしておく方が常に正しい。
fn collect_files_in_commit(dir: &str, sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            // 非 ASCII ファイル名のクォートを無効化 (パス照合を生 UTF-8 名で行うため)
            "-c",
            "core.quotepath=off",
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            sha,
        ])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// 除外 glob マッチャ。
///
/// `globset` を直接依存に追加せず、`ignore::overrides` 経由で組み立てる
/// (既存 refs/dead-code と同じスタイルを踏襲)。
/// ignore の Override は「whitelist」を構築する API なので、除外したいパターンは
/// `!` プレフィクスを付与して登録し、`Match::Ignore` を「除外対象」として判定する。
pub(crate) struct CoChangeExclude {
    inner: ignore::overrides::Override,
}

impl CoChangeExclude {
    pub(crate) fn build(user_globs: &[String]) -> Result<Self> {
        // OverrideBuilder には書き込み可能な root が必要だが、glob マッチング自体は
        // パス文字列で行うため任意のディレクトリで構わない。
        let mut ob = ignore::overrides::OverrideBuilder::new(".");
        for pat in BLAME_DEFAULT_EXCLUDE_GLOBS {
            ob.add(&format!("!{pat}")).map_err(|e| {
                AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!("invalid built-in exclude glob {pat}: {e}"),
                )
            })?;
        }
        for pat in user_globs {
            ob.add(&format!("!{pat}")).map_err(|e| {
                AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!("invalid exclude glob {pat}: {e}"),
                )
            })?;
        }
        let inner = ob
            .build()
            .map_err(|e| AstroError::new(ErrorCode::InvalidRequest, format!("glob build: {e}")))?;
        Ok(Self { inner })
    }

    pub(crate) fn is_match(&self, path: &str) -> bool {
        // `!pattern` で登録 → match すると Match::Ignore が返る (= 除外対象)
        self.inner.matched(path, false).is_ignore()
    }
}

fn build_exclude_matcher(user_globs: &[String]) -> Result<CoChangeExclude> {
    CoChangeExclude::build(user_globs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のヘルパー。git リポジトリを初期化する。
    fn init_repo(repo: &std::path::Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(repo)
                .output()
                .unwrap();
        }
    }

    /// テスト用のヘルパー。ファイル群を書き込んでコミットする。
    fn commit_files(repo: &std::path::Path, files: &[(&str, &str)], msg: &str) {
        for (name, content) in files {
            let path = repo.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(repo)
            .output()
            .unwrap();
    }

    fn opts_with(mutator: impl FnOnce(&mut CoChangeOptions)) -> CoChangeOptions {
        // テストは小さなリポ (typically <= 5 commits) で動かすため、
        // production デフォルト (min_denominator=2, min_samples=2,
        // min_confidence=0.3, smoothing_beta=8.0, commit_size_pivot=8) を
        // 緩めて旧来の挙動 (recall 重視) を維持する。各テストで必要に応じて
        // mutator で再上書きする。
        let mut o = CoChangeOptions {
            min_denominator: 1,
            min_samples: 1,
            min_confidence: 0.0,
            smoothing_beta: 4.0,
            // size weighting も無効化 (旧テストは均等カウント前提)。
            commit_size_pivot: 0,
            // author 圧縮も無効化 (旧テストは raw weighted 集計前提)。
            author_unit_window_days: 0,
            ..CoChangeOptions::default()
        };
        mutator(&mut o);
        o
    }

    // ---- blame mode ----

    /// hunk header の旧側 (start,count) を抽出する。COUNT 省略は 1、COUNT=0 は除外。
    #[test]
    fn parse_hunk_old_ranges_basic() {
        let diff = "diff --git a/foo b/foo\n@@ -10,3 +10,3 @@ ctx\n-x\n+x\n@@ -22 +25 @@ ctx\n-y\n+y\n@@ -100,0 +103,2 @@ ctx\n+a\n+b\n";
        let r = parse_hunk_old_ranges(diff);
        assert_eq!(r, vec![(10u64, 3u64), (22u64, 1u64)]);
    }

    /// blame モード: 起点ファイルの過去変更行に関わるコミットで他ファイルが
    /// 一緒に変わっていれば共起ペアとして検出される。
    #[test]
    fn analyze_cochange_blame_detects_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 起点ファイル a.rs を 3 回コミット。各コミットで b.rs も同時に変更し、
        // c.rs はバラバラに 1 回だけ変更。
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("c.rs", "// solo")], "solo c");
        // HEAD で a.rs を再修正 (起点になる差分を作る)
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();

        // a.rs↔b.rs が検出されるはず (a.rs の以前の編集コミットで b.rs も変わっている)
        let has_pair = result
            .entries
            .iter()
            .any(|e| e.file_a == "a.rs" && e.file_b == "b.rs");
        assert!(
            has_pair,
            "expected a.rs↔b.rs pair, got: {:?}",
            result.entries
        );
        // c.rs は a.rs の blame コミット集合に含まれないので出ない
        assert!(
            result.entries.iter().all(|e| e.file_b != "c.rs"),
            "c.rs should not appear, got: {:?}",
            result.entries
        );
    }

    /// 起点ファイル自身は候補に出ない
    #[test]
    fn analyze_cochange_blame_excludes_source_files_from_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string(), "b.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();

        // 起点が a.rs / b.rs で、両者ともお互いを候補にしてはならない
        for e in &result.entries {
            assert!(
                !(e.file_b == "a.rs" || e.file_b == "b.rs"),
                "source file appeared as candidate: {e:?}"
            );
        }
    }

    /// base 側に存在しない新規追加ファイルは起点にできない。
    ///
    /// blame 対象行が無く、`git log <base> -- <file>` も空 (base 時点でファイルが
    /// 存在しない) なので履歴 fallback も効かない。追加コミット自身を証拠にすると
    /// 「同じコミットの全ファイルが confidence 1.0」になるだけで情報量が無いため、
    /// これは意図した挙動。理由は diagnostics に残す。
    #[test]
    fn analyze_cochange_new_file_has_no_evidence_and_reports_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 何かしら 1 コミット作って HEAD~1 を解決可能にする
        commit_files(repo, &[("seed.rs", "// seed")], "seed");
        // 新規ファイル a.rs を追加 (= base に存在しない、純粋追加)
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "add a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty() && result.commits_analyzed == 0,
            "new-file source should yield empty result, got: {result:?}"
        );
        assert_eq!(result.diagnostics.sources_requested, 1);
        assert_eq!(result.diagnostics.sources_without_evidence, 1);
        assert_eq!(result.diagnostics.new_files_without_history, 1);
        assert!(
            result
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::NewFileHasNoPriorHistory)
        );
    }

    /// 既存ファイルへの純粋追加 (旧 count=0 hunk のみ) は blame 行が取れないが、
    /// 履歴 fallback がファイル自身のコミット履歴を代替証拠にして共変更を出す。
    ///
    /// 「新しいフィールド/メソッドを 1 個足す」という最も頻度の高い変更形が
    /// まるごと不可視だったのを塞ぐ。
    #[test]
    fn analyze_cochange_pure_addition_falls_back_to_file_history() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // a.rs と b.rs を 3 回同時変更して履歴を作る
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // HEAD で a.rs に**行を足すだけ** (旧 count=0 の純粋追加 hunk)
        commit_files(
            repo,
            &[("a.rs", "fn a() { 2 }\nfn added() {}\n")],
            "append a",
        );

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();

        assert!(
            result.entries.iter().any(|e| e.file_b == "b.rs"),
            "純粋追加でも履歴 fallback で b.rs が出るべき: {:?}",
            result.entries
        );
        assert!(
            result
                .entries
                .iter()
                .all(|e| e.evidence == Some(CoChangeEvidence::History)),
            "fallback 由来のエントリは history と識別できるべき: {:?}",
            result.entries
        );
        assert_eq!(result.diagnostics.sources_with_history_evidence, 1);
        assert_eq!(result.diagnostics.sources_with_blame_evidence, 0);
        assert!(
            result
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::NoChangedOldLines)
        );
    }

    /// `--paths <file>` で base..HEAD に差分の無いファイルを指定しても、
    /// 履歴 fallback で「このファイルと一緒に変わるファイル」が返る。
    ///
    /// これがドキュメント記載の主用途 (Files that change together) だが、
    /// 旧実装は変更行が無いと必ず空を返していた。
    #[test]
    fn analyze_cochange_unchanged_paths_falls_back_to_file_history() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // HEAD は a.rs / b.rs に触らない
        commit_files(repo, &[("unrelated.rs", "// x")], "unrelated");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            // base..HEAD に a.rs の差分は無い
            o.base = Some("HEAD~1".to_string());
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.iter().any(|e| e.file_b == "b.rs"),
            "差分の無い --paths でも履歴から共変更が返るべき: {:?}",
            result.entries
        );
        assert_eq!(result.diagnostics.sources_with_history_evidence, 1);
    }

    /// `history_limit = 0` で fallback を無効化すると旧挙動 (blame のみ) に戻る。
    #[test]
    fn analyze_cochange_history_limit_zero_disables_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("unrelated.rs", "// x")], "unrelated");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.history_limit = 0;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty() && result.commits_analyzed == 0,
            "history_limit=0 では fallback せず空: {result:?}"
        );
        assert_eq!(result.diagnostics.sources_without_evidence, 1);
        assert!(
            result
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::NoCommitEvidence)
        );
    }

    /// blame で十分な証拠が取れている起点は fallback せず blame を維持する
    /// (fallback は「証拠が足りないときだけ」の補完であることの固定)。
    #[test]
    fn analyze_cochange_sufficient_blame_evidence_skips_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\nfn a2() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // 既存 2 行をどちらも書き換える → blame 対象行あり
        commit_files(
            repo,
            &[("a.rs", "fn a() { 99 }\nfn a2() { 99 }\n")],
            "edit a",
        );

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_denominator = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(!result.entries.is_empty(), "blame 由来の候補が出ること");
        assert!(
            result.entries.iter().all(|e| e.evidence.is_none()),
            "blame 証拠が足りているので fallback しない: {:?}",
            result.entries
        );
        assert_eq!(result.diagnostics.sources_with_blame_evidence, 1);
        assert_eq!(result.diagnostics.sources_with_history_evidence, 0);
    }

    /// `max_files_per_commit` 超過でスキップしたコミットは confidence の分母からも外す。
    ///
    /// 旧実装は分母に証拠集合サイズをそのまま使っていたため、co (超過コミットを
    /// 含まない) と母数がずれ confidence が系統的に過小評価されていた。
    #[test]
    fn confidence_denominator_excludes_oversized_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 通常コミット 2 回 (a.rs + b.rs)
        for i in 0..2 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\nfn a2() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // 巨大コミット 1 回 (a.rs の別行 + 大量ファイル) → max_files_per_commit で除外させる
        let mut big: Vec<(String, String)> = vec![(
            "a.rs".to_string(),
            "fn a() { 1 }\nfn a2() { 9 }\n".to_string(),
        )];
        for i in 0..12 {
            big.push((format!("bulk{i}.rs"), format!("// {i}\n")));
        }
        let big_refs: Vec<(&str, &str)> =
            big.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        commit_files(repo, &big_refs, "bulk");
        // HEAD で a.rs の 2 行とも書き換え、blame が上記 3 コミットに散るようにする
        commit_files(repo, &[("a.rs", "fn a() { x }\nfn a2() { x }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            // 13 ファイルの bulk コミットを除外する
            o.max_files_per_commit = 5;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let entry = result
            .entries
            .iter()
            .find(|e| e.file_b == "b.rs")
            .unwrap_or_else(|| panic!("b.rs が候補に出るべき: {:?}", result.entries));
        // bulk コミットは分母から外れるので denominator は blame 集合サイズより小さい
        assert!(
            entry.denominator.unwrap() < entry.total_changes_a,
            "除外コミットの分だけ実効分母が小さくなるべき: denom={:?} evidence_set={}",
            entry.denominator,
            entry.total_changes_a
        );
        assert!(
            (entry.confidence - 1.0).abs() < 1e-9,
            "集計対象コミットでは常に共変更しているので confidence=1.0: {entry:?}"
        );
    }

    /// 既定除外 glob (vendor/ 等) に該当する候補は出ない
    #[test]
    fn analyze_cochange_blame_default_excludes_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("vendor/lib.php", &format!("// {i}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result
                .entries
                .iter()
                .all(|e| !e.file_b.starts_with("vendor/")),
            "vendor/ should be excluded by default, got: {:?}",
            result.entries
        );
    }

    /// max_source_files を超える起点指定は InvalidRequest で停止する
    #[test]
    fn analyze_cochange_blame_rejects_when_source_files_exceeds_limit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("seed.rs", "// seed")], "seed");

        let opts = opts_with(|o| {
            // 3 件起点 / 上限 2 → reject
            o.source_files = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
            o.max_source_files = 2;
            o.base = Some("HEAD".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let err = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max-source-files") || msg.contains("max_source_files"),
            "error message should mention the limit, got: {msg}"
        );
    }

    /// max_source_files = 0 は無制限なので、件数が多くても通る
    #[test]
    fn analyze_cochange_blame_unlimited_when_max_source_files_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");
        commit_files(repo, &[("a.rs", "fn a() { 1 }\n")], "edit");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.max_source_files = 0; // unlimited
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        // エラーにならず Ok で返ること (entries の中身は問わない)
        let _ = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
    }

    /// rename フラグ ON のときファイルが rename されていても以前の編集が blame で辿れる。
    /// blame -M がない (rename=false) と HEAD~1 base 時点の旧ファイル名側に履歴が消える。
    #[test]
    fn analyze_cochange_blame_rename_recovers_history_across_move() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 旧名 old.rs を 3 回 b.rs と同時編集して履歴を作る
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("old.rs", &format!("fn x() {{ {i} }}\nfn y() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // git mv で rename
        std::process::Command::new("git")
            .args(["mv", "old.rs", "new.rs"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "rename"])
            .current_dir(repo)
            .output()
            .unwrap();
        // rename 後の new.rs を更に変更 (起点となる diff)
        commit_files(
            repo,
            &[("new.rs", "fn x() { 99 }\nfn y() { 99 }\n")],
            "edit",
        );

        let mut opts = opts_with(|o| {
            o.source_files = vec!["new.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        // rename=true: old.rs の旧履歴を辿れて b.rs と共起検出されるはず
        opts.rename = true;
        let with_rename = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let detected_rename = with_rename.entries.iter().any(|e| e.file_b == "b.rs");
        assert!(
            detected_rename,
            "rename=true should let blame follow old.rs and find b.rs co-change, got: {:?}",
            with_rename.entries,
        );
    }

    /// ignore_merges=true でマージコミットは blame コミット集合から除外される
    #[test]
    fn analyze_cochange_blame_ignore_merges_drops_merge_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 共通 base コミット
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");
        // feature ブランチで a.rs と b.rs を編集
        std::process::Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(repo)
            .output()
            .unwrap();
        commit_files(
            repo,
            &[("a.rs", "fn a() { 1 }\n"), ("b.rs", "fn b() {}\n")],
            "feature edit",
        );
        // main に戻ってマージ (no-ff でマージコミットを作る)
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["merge", "--no-ff", "feature", "-m", "merge feature"])
            .current_dir(repo)
            .output()
            .unwrap();
        // 起点となる差分: a.rs を更に変更
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let mut opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.ignore_merges = false;
        });

        // 比較: ignore_merges=false / true で commits_analyzed が増減すること
        let baseline = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        opts.ignore_merges = true;
        let filtered = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            filtered.commits_analyzed <= baseline.commits_analyzed,
            "ignore_merges should not increase commits_analyzed: baseline={} filtered={}",
            baseline.commits_analyzed,
            filtered.commits_analyzed,
        );
    }

    /// max_blame_commits を超える SHA 集合は InvalidRequest で停止する
    #[test]
    fn analyze_cochange_blame_rejects_when_blame_commit_set_exceeds_limit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // 各行を別コミットで編集して blame の SHA 集合が複数 (3 件) になる状況を作る。
        // 単に同じ行を上書きする履歴だと blame は最後の SHA 1 件しか返さないので
        // 3 行構成にして 1 行ずつ別コミットで編集する。
        commit_files(
            repo,
            &[("a.rs", "fn x1() {}\nfn x2() {}\nfn x3() {}\n")],
            "init",
        );
        commit_files(
            repo,
            &[("a.rs", "fn x1() { 1 }\nfn x2() {}\nfn x3() {}\n")],
            "edit x1",
        );
        commit_files(
            repo,
            &[("a.rs", "fn x1() { 1 }\nfn x2() { 2 }\nfn x3() {}\n")],
            "edit x2",
        );
        commit_files(
            repo,
            &[("a.rs", "fn x1() { 1 }\nfn x2() { 2 }\nfn x3() { 3 }\n")],
            "edit x3",
        );
        // HEAD: 全 3 行を変更 (= hunk の旧側に 3 行入り、それぞれの blame SHA が異なる)
        commit_files(
            repo,
            &[("a.rs", "fn x1() { 9 }\nfn x2() { 9 }\nfn x3() { 9 }\n")],
            "edit all",
        );

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.max_blame_commits = 1; // SHA 集合 3 件 > 上限 1 で停止
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let err = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max-blame-commits") || msg.contains("max_blame_commits"),
            "error should mention the limit, got: {msg}"
        );
    }

    /// max_blame_commits = 0 は無制限で従来挙動と一致
    #[test]
    fn analyze_cochange_blame_unlimited_when_max_blame_commits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("fn a() {{ {i} }}\n"))],
                &format!("e{i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~3".to_string());
            o.max_blame_commits = 0; // unlimited
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        // エラー無く Ok で返ること
        let _ = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
    }

    /// timeout_secs を極小 (1秒) にしても、テスト用の小規模リポは1秒以内で終わる。
    /// したがってここでは「タイムアウト機構が走っても通常完走する」ことを確認する
    /// (タイムアウト発火そのものは決定論的に再現できないため)。
    #[test]
    fn analyze_cochange_blame_timeout_short_does_not_abort_small_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");
        commit_files(repo, &[("a.rs", "fn a() { 1 }\n")], "edit");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.timeout_secs = 60; // 十分大きな上限
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let _ = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
    }

    /// timeout_secs = 0 は無制限。
    #[test]
    fn analyze_cochange_blame_unlimited_when_timeout_secs_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");
        commit_files(repo, &[("a.rs", "fn a() { 1 }\n")], "edit");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.timeout_secs = 0; // unlimited
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let _ = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
    }

    /// --copy フラグは git blame に -C を追加する。
    /// 機能的には `git mv old.rs new.rs` 後の old.rs 由来行が copy 検出で辿れる
    /// (rename と copy の両方が成立するケースだが、ここでは copy 単体の動作を確認する)。
    #[test]
    fn analyze_cochange_blame_copy_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 元ファイル orig.rs を作って b.rs と一緒に何度か編集
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("orig.rs", &format!("fn shared() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // orig.rs の中身を copy.rs にコピー (cp 相当を新規追加で再現)
        commit_files(repo, &[("copy.rs", "fn shared() { 0 }\n")], "copy");
        // copy.rs を変更 (起点 diff)
        commit_files(repo, &[("copy.rs", "fn shared() { 99 }\n")], "edit copy");

        let opts = opts_with(|o| {
            o.source_files = vec!["copy.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.copy = true; // -C 有効化
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        // エラーなく完走することを最低限確認する
        // (-C 検出の再現性はテスト環境/git バージョン依存があるため、
        //  ここでは「-C を渡しても crash しない」ことだけ保証する)
        let _ = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
    }

    // ---- noise reduction (smoothing / min_denominator / per_source_limit) ----

    /// blame リポを 1 起点 1 共起ペアで作る簡易ヘルパ。
    /// `pairs` で指定した (source, candidate) を `iters` 回ずつ別コミットで一緒に変更し、
    /// 最後に source を 1 度だけ変更して起点 diff を作る。
    fn build_pair_repo(repo: &std::path::Path, pairs: &[(&str, &str, usize)], source_name: &str) {
        init_repo(repo);
        for (s, c, iters) in pairs {
            for i in 0..*iters {
                commit_files(
                    repo,
                    &[
                        (*s, &format!("fn {}() {{ {i} }}\n", *s)),
                        (*c, &format!("fn {}() {{ {i} }}\n", *c)),
                    ],
                    &format!("pair {s}/{c} #{i}"),
                );
            }
        }
        // 起点 diff
        commit_files(
            repo,
            &[(source_name, &format!("fn {source_name}() {{ 99 }}\n"))],
            "edit source",
        );
    }

    /// Bayesian smoothing 有効: co=1/denom=1 の score は (1+α)/(1+α+β) になり、1.0 ではない。
    #[test]
    fn analyze_cochange_blame_smoothing_lowers_singleton_pair() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        // a.rs と b.rs を 1 回だけ同時変更 → blame 集合 1 件、co=1, denom=1
        build_pair_repo(repo, &[("a", "b", 1)], "a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.smoothing_alpha = 1.0;
            o.smoothing_beta = 4.0;
            o.disable_smoothing = false;
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let entry = result
            .entries
            .iter()
            .find(|e| e.file_a == "a" && e.file_b == "b")
            .expect("a↔b pair should exist");
        // confidence は raw 1.0、score は (1+1)/(1+1+4) = 0.333...
        assert!(
            (entry.confidence - 1.0).abs() < 1e-9,
            "raw confidence = 1.0"
        );
        let score = entry.score.expect("score must be Some in blame mode");
        assert!(
            (score - (2.0_f64 / 6.0)).abs() < 1e-9,
            "smoothed score = (1+1)/(1+1+4) ≈ 0.333, got {score}"
        );
    }

    /// `--no-smoothing` (disable_smoothing=true): score == confidence で互換維持。
    #[test]
    fn analyze_cochange_blame_no_smoothing_returns_raw_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        build_pair_repo(repo, &[("a", "b", 2)], "a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.disable_smoothing = true;
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        for e in &result.entries {
            let s = e.score.expect("score is Some even with --no-smoothing");
            assert!(
                (s - e.confidence).abs() < 1e-9,
                "no-smoothing: score == confidence, got s={s} conf={}",
                e.confidence,
            );
        }
    }

    /// `min_denominator >= 2`: 起点 blame 集合が 1 件しかない起点はスキップされる。
    #[test]
    fn analyze_cochange_blame_min_denominator_filters_small_sets() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        // a と b を 1 回だけ同時変更 → blame 集合 1 件
        build_pair_repo(repo, &[("a", "b", 1)], "a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_denominator = 2; // 1 件しかない起点は除外
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty(),
            "min_denominator=2 should drop denom=1 source, got: {:?}",
            result.entries
        );
    }

    /// 起点 `a` の各行を別 commit で `b/c/d` と一緒に変更し、複数候補を作る共通ヘルパ。
    /// 構造: HEAD~1 時点で a は 3 行 (line1=a-b 共起, line2=a-c 共起, line3=a-d 共起)。
    /// HEAD で全 3 行を上書きすれば、blame 旧側の 3 行が 3 SHA に紐づき、
    /// 候補 b/c/d がそれぞれ co=1 で per_source に入る。
    fn build_multi_candidate_repo(repo: &std::path::Path) {
        init_repo(repo);
        // i=0: a に line1 と b を同時 add
        commit_files(repo, &[("a", "line1\n"), ("b", "fn b() {}\n")], "pair a-b");
        // i=1: a に line2 を追加 + c を同時 add
        commit_files(
            repo,
            &[("a", "line1\nline2\n"), ("c", "fn c() {}\n")],
            "pair a-c",
        );
        // i=2: a に line3 を追加 + d を同時 add
        commit_files(
            repo,
            &[("a", "line1\nline2\nline3\n"), ("d", "fn d() {}\n")],
            "pair a-d",
        );
        // HEAD: a の全 3 行を上書き → 旧側 3 行 + 各行が別 SHA で blame される
        commit_files(repo, &[("a", "x1\nx2\nx3\n")], "edit a all lines");
    }

    /// `per_source_limit = N`: 起点ごと候補上位 N 件に絞られる。
    #[test]
    fn analyze_cochange_blame_per_source_limit_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        build_multi_candidate_repo(repo);

        let opts = opts_with(|o| {
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.per_source_limit = 1; // 候補 1 件まで
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let from_a: Vec<_> = result.entries.iter().filter(|e| e.file_a == "a").collect();
        assert_eq!(
            from_a.len(),
            1,
            "per_source_limit=1 should keep only 1 candidate per source, got: {:?}",
            from_a
        );
    }

    /// per_source_limit = 0 は無制限 (= 既存挙動)。
    #[test]
    fn analyze_cochange_blame_per_source_limit_zero_is_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        build_multi_candidate_repo(repo);

        let opts = opts_with(|o| {
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.per_source_limit = 0;
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let from_a: Vec<_> = result.entries.iter().filter(|e| e.file_a == "a").collect();
        assert!(
            from_a.len() >= 2,
            "per_source_limit=0 should keep multiple candidates, got: {:?}",
            from_a,
        );
    }

    /// blame モードで base がオプションプレフィクスで始まる場合は拒否する。
    /// `--paths`/`--paths-file` 経由で `resolve_blame_source_files` の検証を
    /// 迂回しても、`analyze_cochange` 自身が validate_revision で停止する
    /// ことを保証する。`git diff/blame` への `--output=...` 等のオプション混入を防ぐ。
    #[test]
    fn analyze_cochange_blame_rejects_dash_prefixed_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("--output=/tmp/pwn".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let err = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--base") && msg.contains("must not start with"),
            "error message should reject `-` prefixed base, got: {msg}"
        );
    }

    /// blame モードで base に NUL バイトが含まれる場合は拒否する。
    #[test]
    fn analyze_cochange_blame_rejects_nul_in_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "init");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD\0foo".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let err = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--base") && msg.contains("NUL"),
            "error message should reject NUL in base, got: {msg}"
        );
    }

    /// validate_revision のユニットテスト
    #[test]
    fn validate_revision_accepts_normal_refs() {
        assert!(validate_revision("HEAD", "--base").is_ok());
        assert!(validate_revision("HEAD~3", "--base").is_ok());
        assert!(validate_revision("main", "--base").is_ok());
        assert!(validate_revision("origin/main", "--base").is_ok());
        assert!(validate_revision("v1.0.0", "--base").is_ok());
        assert!(validate_revision("abc1234", "--base").is_ok());
    }

    #[test]
    fn validate_revision_rejects_invalid_refs() {
        assert!(validate_revision("", "--base").is_err());
        assert!(validate_revision("--output=/tmp/pwn", "--base").is_err());
        assert!(validate_revision("-p", "--base").is_err());
        assert!(validate_revision("HEAD\0foo", "--base").is_err());
    }

    // ---- precision tests (commit-size weighting) ----

    #[test]
    fn commit_size_weight_returns_full_for_small_commits() {
        // file_count <= pivot のときは 1.0 (=フル重み)
        assert!((commit_size_weight(1, 8, 100) - 1.0).abs() < 1e-9);
        assert!((commit_size_weight(8, 8, 100) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn commit_size_weight_decays_for_large_commits() {
        // file_count > pivot のときは sqrt(pivot/file_count) で減衰
        let w16 = commit_size_weight(16, 8, 100);
        let w32 = commit_size_weight(32, 8, 100);
        assert!((w16 - (8.0_f64 / 16.0).sqrt()).abs() < 1e-9);
        assert!((w32 - (8.0_f64 / 32.0).sqrt()).abs() < 1e-9);
        assert!(w16 > w32, "larger commits must have lower weight");
        assert!(w32 < 1.0);
    }

    #[test]
    fn commit_size_weight_is_zero_above_hard_cap() {
        // hard_max を超えるコミットは 0.0 (= 完全スキップ)
        assert_eq!(commit_size_weight(101, 8, 100), 0.0);
        assert_eq!(commit_size_weight(1000, 8, 100), 0.0);
    }

    #[test]
    fn commit_size_weight_pivot_zero_disables_weighting() {
        // pivot=0 で size weighting 無効化、常に 1.0 (hard cap 適用後)
        assert_eq!(commit_size_weight(1, 0, 100), 1.0);
        assert_eq!(commit_size_weight(50, 0, 100), 1.0);
        assert_eq!(commit_size_weight(101, 0, 100), 0.0); // hard cap は効く
    }

    /// co=2/denom=2 (confidence 1.0) は production デフォルトで**出力される**。
    ///
    /// 旧実装は min_confidence を smoothing 済み score と突き合わせており、
    /// score=(2+1)/(2+1+8)≈0.273 < 0.3 で 100% 共変更しているペアを落としていた。
    /// score の上限は `(d+α)/(d+α+β)` なので、既定 β=8 では分母 3 未満の起点が
    /// 構造的に出力不能になる (実リポジトリでは変更行 blame の分母は 1〜4 が大半)。
    /// 閾値は raw confidence に、score はランキングに、と役割を分離した。
    #[test]
    fn small_sample_pair_is_emitted_by_default_and_gated_by_min_score() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // 過去 2 commit で a.rs と b.rs を同時変更 (a.rs を起点に co=2/denom=2)
        for i in 0..2 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = CoChangeOptions {
            source_files: vec!["a.rs".to_string()],
            base: Some("HEAD~1".to_string()),
            // production デフォルトを使う
            ..CoChangeOptions::default()
        };
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert_eq!(
            result.entries.len(),
            1,
            "confidence=1.0 のペアは既定で出力されるべき: {:?}",
            result.entries
        );
        let entry = &result.entries[0];
        assert_eq!(entry.file_b, "b.rs");
        assert!((entry.confidence - 1.0).abs() < 1e-9);
        // score はランキング用の shrinkage 値として残る (0.3 未満だが出力は止めない)
        let score = entry.score.expect("score は常に付く");
        assert!(score < 0.3, "既定 β=8 では score は 0.3 未満: {score}");

        // score ゲートを明示的に有効にしたときだけ落ちる
        let gated = analyze_cochange(
            repo.to_str().unwrap(),
            &CoChangeOptions {
                min_score: 0.3,
                ..opts.clone()
            },
        )
        .unwrap();
        assert!(
            gated.entries.is_empty(),
            "--min-score 0.3 では score<0.3 のペアが落ちる: {:?}",
            gated.entries
        );
        assert_eq!(gated.diagnostics.filtered_min_score, 1);
        assert!(
            gated
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::BelowMinScore)
        );
    }

    /// confidence が閾値未満のペアは既定でも落ちる (閾値の意味が raw 比率であること)。
    #[test]
    fn low_confidence_pair_is_filtered_by_min_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // a.rs は 5 commit、うち b.rs と同時変更は 1 commit だけ → confidence 0.2
        commit_files(
            repo,
            &[("a.rs", "fn a() { 0 }\n"), ("b.rs", "fn b() { 0 }\n")],
            "pair 0",
        );
        for i in 1..5 {
            commit_files(repo, &[("a.rs", &format!("fn a() {{ {i} }}\n"))], "solo");
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = CoChangeOptions {
            source_files: vec!["a.rs".to_string()],
            base: Some("HEAD~1".to_string()),
            // co=1 でも候補に上がるようにして、落ちる理由を min_confidence に限定する
            min_samples: 1,
            ..CoChangeOptions::default()
        };
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty(),
            "confidence 0.2 < 0.3 は落ちるべき: {:?}",
            result.entries
        );
        assert!(result.diagnostics.filtered_min_confidence >= 1);
        assert!(
            result
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::BelowMinConfidence)
        );
    }

    /// smoothing の on/off は**候補集合を変えず順序だけ**を変える。
    /// (旧実装は smoothing が閾値そのものを動かしていた)
    #[test]
    fn smoothing_toggle_changes_order_not_membership() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                    ("c.rs", &format!("fn c() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let base_opts = CoChangeOptions {
            source_files: vec!["a.rs".to_string()],
            base: Some("HEAD~1".to_string()),
            ..CoChangeOptions::default()
        };
        let on = analyze_cochange(repo.to_str().unwrap(), &base_opts).unwrap();
        let off = analyze_cochange(
            repo.to_str().unwrap(),
            &CoChangeOptions {
                disable_smoothing: true,
                ..base_opts.clone()
            },
        )
        .unwrap();

        let mut on_files: Vec<&str> = on.entries.iter().map(|e| e.file_b.as_str()).collect();
        let mut off_files: Vec<&str> = off.entries.iter().map(|e| e.file_b.as_str()).collect();
        on_files.sort_unstable();
        off_files.sort_unstable();
        assert!(!on_files.is_empty(), "候補が出ること: {:?}", on.entries);
        assert_eq!(
            on_files, off_files,
            "smoothing の on/off で候補集合は変わらない"
        );
    }

    /// commit-size weighting: 同じ raw co でも、小コミット由来のペアが
    /// 大コミット由来のペアより高 score になる。
    #[test]
    fn commit_size_weighting_ranks_focused_commits_higher() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // blame は base 時点のファイルスナップショットを見るので、base 以前の
        // コミットが各行を書いた履歴を残しておく必要がある。
        //
        // commit A (initial): src.rs に line1
        // commit B (focused): src.rs に line2 を追加 + small.rs (= 2 ファイル)
        // commit C (bulk):    src.rs に line3 を追加 + bulk_0..bulk_19.rs (= 21 ファイル)
        // commit D (last):    src.rs を全行書き換え (起点となる差分を作る)
        // base = HEAD~1 = commit C 時点。base の src.rs に A/B/C 由来の行が残り、
        // blame をかけると blame 集合 = {A, B, C} になる。
        commit_files(repo, &[("src.rs", "// l1 a\n")], "initial");
        commit_files(
            repo,
            &[("src.rs", "// l1 a\n// l2 b\n"), ("small.rs", "// v0\n")],
            "focused",
        );
        let mut bulk_files: Vec<(String, String)> = vec![(
            "src.rs".to_string(),
            "// l1 a\n// l2 b\n// l3 c\n".to_string(),
        )];
        for i in 0..20 {
            bulk_files.push((format!("bulk_{i}.rs"), format!("// {i}\n")));
        }
        let bulk_refs: Vec<(&str, &str)> = bulk_files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        commit_files(repo, &bulk_refs, "bulk refactor");
        commit_files(repo, &[("src.rs", "// new\n")], "edit src");

        let opts = CoChangeOptions {
            source_files: vec!["src.rs".to_string()],
            // base = HEAD~1 = bulk commit 後。base 時点の src.rs に A/B/C の行が残る。
            base: Some("HEAD~1".to_string()),
            min_confidence: 0.0,
            min_samples: 1,
            min_denominator: 1,
            commit_size_pivot: 8,
            // author 圧縮を無効化 (テスト config では全 commit が同 author なので、
            // window > 0 だと unit=1 に圧縮されて denom が 1 になる)
            author_unit_window_days: 0,
            smoothing_beta: 4.0,
            ..CoChangeOptions::default()
        };
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();

        let small = result
            .entries
            .iter()
            .find(|e| e.file_b == "small.rs")
            .expect("small.rs should be detected (focused commit)");
        let bulk = result
            .entries
            .iter()
            .find(|e| e.file_b == "bulk_0.rs")
            .expect("bulk_0.rs should be detected (bulk commit)");
        assert!(
            small.score.unwrap() > bulk.score.unwrap(),
            "focused commit pair must rank higher than bulk pair: small={small:?}, bulk={bulk:?}",
        );
    }

    /// pivot=0 (size weighting 無効) のとき、score は raw co/denom の smoothing と一致する。
    #[test]
    fn commit_size_pivot_zero_matches_legacy_score() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..2 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("p{i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = CoChangeOptions {
            source_files: vec!["a.rs".to_string()],
            base: Some("HEAD~1".to_string()),
            min_confidence: 0.0,
            min_samples: 1,
            min_denominator: 1,
            commit_size_pivot: 0, // 旧挙動 (size weighting 無効)
            smoothing_alpha: 1.0,
            smoothing_beta: 4.0,
            // author 圧縮を無効化 (旧挙動再現のため)
            author_unit_window_days: 0,
            ..CoChangeOptions::default()
        };
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let entry = result
            .entries
            .iter()
            .find(|e| e.file_b == "b.rs")
            .expect("a.rs↔b.rs should be detected");
        // HEAD~1 vs HEAD の a.rs diff の旧側 (= HEAD~1 の a.rs 1 行) に
        // blame をかけると、その行は p1 commit で書かれたものなので
        // blame 集合は 1 commit (p1) のみになる。p1 では b.rs も同時変更されているので
        // co=1, denom=1, α=1, β=4: score = (1+1)/(1+1+4) = 2/6 = 1/3 ≈ 0.3333
        let expected = 1.0_f64 / 3.0;
        assert!(
            (entry.score.unwrap() - expected).abs() < 1e-9,
            "pivot=0 should reproduce legacy score: got {:?}, expected {expected}",
            entry.score
        );
    }

    /// tie-break: ranking 値が同点なら co_changes 降順を優先する。
    /// path 昇順だけでは「低 score 帯で co=2 の弱いペアが co=10 のペアより上に来る」
    /// 不自然な順序が起きるが、co_changes 降順を入れることで解消される。
    #[test]
    fn tie_break_prefers_higher_co_changes_then_confidence() {
        // 同 score を作るために、smoothing 無効 + 同 confidence のペアを 2 つ作る。
        // confidence = co/denom が同じになるよう co=10/denom=20 と co=2/denom=4 にする。
        let high_co = CoChangeEntry {
            file_a: "z.rs".into(),
            file_b: "y.rs".into(),
            co_changes: 10,
            total_changes_a: 20,
            total_changes_b: 10,
            confidence: 0.5,
            denominator: Some(20),
            score: Some(0.5),
            evidence: None,
        };
        let low_co = CoChangeEntry {
            file_a: "a.rs".into(),
            file_b: "b.rs".into(),
            co_changes: 2,
            total_changes_a: 4,
            total_changes_b: 2,
            confidence: 0.5,
            denominator: Some(4),
            score: Some(0.5),
            evidence: None,
        };

        // smoothing on/off の両方で同じ tie-break が効くことを確認
        for smoothing_on in [true, false] {
            let mut entries = [low_co.clone(), high_co.clone()];
            entries.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));
            assert_eq!(
                entries[0].co_changes, 10,
                "higher co_changes must come first (smoothing_on={smoothing_on})"
            );
            assert_eq!(entries[1].co_changes, 2);
        }
    }

    /// author 圧縮: 同一 author の連続 commit が 1 unit に圧縮されることで、
    /// raw co_changes=2/denom=3 が unit ベース denom=1 に縮小し、score が下がる。
    /// blame は base 時点の起点ファイルを走査するため、base 時点で各 commit の貢献が
    /// 1 行ずつ残るように行追加で履歴を作る。
    #[test]
    fn author_unit_window_compresses_same_author_burst() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // commit A: a.rs line 1 を書く (b.rs 無関係)
        // commit B: a.rs line 2 を追加 + b.rs を変更
        // commit C: a.rs line 3 を追加 + b.rs を変更
        // commit D (last): a.rs を全行書き換え (起点 diff)
        // base=HEAD~1=C, blame で {A,B,C} の 3 commit、b.rs と共起するのは B,C
        commit_files(repo, &[("a.rs", "// l1\n")], "A");
        commit_files(repo, &[("a.rs", "// l1\n// l2\n"), ("b.rs", "v0\n")], "B");
        commit_files(
            repo,
            &[("a.rs", "// l1\n// l2\n// l3\n"), ("b.rs", "v1\n")],
            "C",
        );
        commit_files(repo, &[("a.rs", "// new\n")], "edit a");

        // window=0 (旧挙動): raw weighted。co=2, denom=3 (initial 含む)、α=1, β=4
        // → score = (2+1)/(3+1+4) = 3/8 = 0.375
        let opts_legacy = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.author_unit_window_days = 0;
        });
        let r_legacy = analyze_cochange(repo.to_str().unwrap(), &opts_legacy).unwrap();
        let s_legacy = r_legacy
            .entries
            .iter()
            .find(|e| e.file_b == "b.rs")
            .map(|e| e.score.unwrap())
            .expect("b.rs should be detected with legacy weighting");

        // window=7: 同 author × 同 week で 1 unit に圧縮 → denom_units=1, co_units=1
        // → score = (1+1)/(1+1+4) = 2/6 = 1/3 ≈ 0.333
        let opts_unit = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.author_unit_window_days = 7;
        });
        let r_unit = analyze_cochange(repo.to_str().unwrap(), &opts_unit).unwrap();
        let s_unit = r_unit
            .entries
            .iter()
            .find(|e| e.file_b == "b.rs")
            .map(|e| e.score.unwrap())
            .expect("b.rs should still be detected with unit compression");

        assert!(
            s_unit < s_legacy,
            "author-unit compression must lower score: legacy={s_legacy}, unit={s_unit}"
        );
    }

    /// author 圧縮: window=0 (default の旧挙動) では `denom_units` / `co_units` が
    /// 集計されず、score は従来どおり raw weighted のみで決まる。
    #[test]
    fn author_unit_window_zero_keeps_legacy_score() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("p{i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.author_unit_window_days = 0;
            o.smoothing_beta = 4.0;
            o.commit_size_pivot = 0;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        let entry = result
            .entries
            .iter()
            .find(|e| e.file_b == "b.rs")
            .expect("b.rs should be detected");
        // co=1, denom=1, α=1, β=4: score = (1+1)/(1+1+4) = 1/3
        let expected = 1.0_f64 / 3.0;
        assert!(
            (entry.score.unwrap() - expected).abs() < 1e-9,
            "window=0 should reproduce legacy weighted score: got {:?}",
            entry.score
        );
    }

    /// blame porcelain の `author-mail` / `author-time` を抽出できる。
    #[test]
    fn parse_blame_porcelain_extracts_author_metadata() {
        let blame = "\
abcd1234567890abcd1234567890abcd12345678 1 1 1
author Test User
author-mail <test@example.com>
author-time 1700000000
author-tz +0900
committer Test User
committer-mail <test@example.com>
committer-time 1700000000
committer-tz +0900
summary p0
filename a.rs
\tfn a() {}
fedc1234567890abcd1234567890abcd00000000 2 2 1
author Other
author-mail <other@example.com>
author-time 1701000000
filename a.rs
\tfn b() {}
";
        let parsed = parse_blame_porcelain(blame).unwrap();
        assert_eq!(parsed.len(), 2);
        let info = parsed
            .get("abcd1234567890abcd1234567890abcd12345678")
            .unwrap();
        assert_eq!(info.author_mail.as_deref(), Some("test@example.com"));
        assert_eq!(info.author_time, Some(1_700_000_000));
        let info2 = parsed
            .get("fedc1234567890abcd1234567890abcd00000000")
            .unwrap();
        assert_eq!(info2.author_mail.as_deref(), Some("other@example.com"));
    }

    /// tie-break: ranking 値・co_changes 同点なら confidence 降順
    #[test]
    fn tie_break_prefers_higher_confidence_when_co_changes_equal() {
        let high_conf = CoChangeEntry {
            file_a: "z.rs".into(),
            file_b: "y.rs".into(),
            co_changes: 5,
            total_changes_a: 5,
            total_changes_b: 5,
            confidence: 1.0,
            denominator: Some(5),
            score: Some(0.4),
            evidence: None,
        };
        let low_conf = CoChangeEntry {
            file_a: "a.rs".into(),
            file_b: "b.rs".into(),
            co_changes: 5,
            total_changes_a: 10,
            total_changes_b: 10,
            confidence: 0.5,
            denominator: Some(10),
            score: Some(0.4),
            evidence: None,
        };

        let mut entries = [low_conf.clone(), high_conf.clone()];
        entries.sort_by(|a, b| compare_entries_by_ranking(a, b, true));
        assert!(
            (entries[0].confidence - 1.0).abs() < 1e-9,
            "higher confidence must come first when score and co_changes tie"
        );
    }
}
