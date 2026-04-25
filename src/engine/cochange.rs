use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{
    BLAME_DEFAULT_EXCLUDE_GLOBS, CoChangeEntry, CoChangeOptions, CoChangeResult,
};

/// `git log` から共変更パターンを解析する。
///
/// フィルタの適用順序:
///   1. merge-base 打ち切り（有効時）
///   2. lookback による走査コミット数制限
///   3. `max_files_per_commit` を超えるコミットの除外
///   4. `min_samples` 未満のペアの除外
///   5. `min_confidence` 未満のペアの除外
///   6. `skip_deleted_files` (HEAD ツリーに存在しないファイルを含むペアの除外)
///   7. `filter_file` による絞り込み
pub fn analyze_cochange(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    // merge-base 打ち切り: 有効かつ算出可能な場合のみコミット範囲を限定する
    let base_revision = if opts.bounded_by_merge_base {
        resolve_merge_base(dir)
    } else {
        None
    };

    // git log のコミット履歴を取得する
    let commits = run_git_log(dir, opts.lookback, base_revision.as_deref())?;
    let commits_analyzed = commits.len();

    // 各ファイルの変更回数をカウントする
    let mut file_counts: HashMap<String, usize> = HashMap::new();
    for commit in &commits {
        for file in commit {
            if let Some(count) = file_counts.get_mut(file.as_str()) {
                *count += 1;
            } else {
                file_counts.insert(file.clone(), 1);
            }
        }
    }

    // 同一コミット内のファイルペアをカウントする。max_files_per_commit を
    // 超えるコミットは初期化・大量生成系としてスキップする。
    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
    for commit in &commits {
        if commit.len() < 2 || commit.len() > opts.max_files_per_commit {
            continue;
        }
        let mut files: Vec<&String> = commit.iter().collect();
        files.sort();
        files.dedup();
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let key = (files[i].clone(), files[j].clone());
                *pair_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    // skip_deleted_files 用に HEAD ツリーのファイル集合を 1 回だけ取得する
    let head_tree: Option<HashSet<String>> = if opts.skip_deleted_files {
        list_head_tree(dir).ok()
    } else {
        None
    };

    let mut entries: Vec<CoChangeEntry> = pair_counts
        .into_iter()
        .filter_map(|((file_a, file_b), co_changes)| {
            if co_changes < opts.min_samples {
                return None;
            }

            let total_a = *file_counts.get(&file_a).unwrap_or(&0);
            let total_b = *file_counts.get(&file_b).unwrap_or(&0);
            let max_total = total_a.max(total_b);
            if max_total == 0 {
                return None;
            }

            let confidence = co_changes as f64 / max_total as f64;
            if confidence < opts.min_confidence {
                return None;
            }

            if let Some(tree) = head_tree.as_ref()
                && (!tree.contains(&file_a) || !tree.contains(&file_b))
            {
                return None;
            }

            if let Some(filter) = opts.filter_file.as_deref()
                && file_a != filter
                && file_b != filter
            {
                return None;
            }

            Some(CoChangeEntry {
                file_a,
                file_b,
                co_changes,
                total_changes_a: total_a,
                total_changes_b: total_b,
                confidence,
                denominator: None,
                score: None,
            })
        })
        .collect();

    entries.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(CoChangeResult {
        entries,
        commits_analyzed,
    })
}

/// `git log --name-only` を実行しコミットごとの変更ファイル一覧を返す。
/// `base_revision` が Some の場合、その履歴からのみ辿る。
fn run_git_log(
    dir: &str,
    lookback: usize,
    base_revision: Option<&str>,
) -> Result<Vec<Vec<String>>> {
    let mut args: Vec<String> = vec![
        "log".into(),
        "--name-only".into(),
        "--pretty=format:---COMMIT---".into(),
        "-n".into(),
        lookback.to_string(),
    ];
    if let Some(rev) = base_revision {
        args.push(rev.to_string());
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git log failed: {stderr}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits: Vec<Vec<String>> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "---COMMIT---" {
            if !current_files.is_empty() {
                commits.push(std::mem::take(&mut current_files));
            }
        } else if !trimmed.is_empty() {
            current_files.push(trimmed.to_string());
        }
    }
    if !current_files.is_empty() {
        commits.push(current_files);
    }
    Ok(commits)
}

/// `git merge-base HEAD <default-branch>` を算出する。
/// 算出できない場合（デフォルトブランチ不明、HEAD が単独、等）は None を返す。
fn resolve_merge_base(dir: &str) -> Option<String> {
    let default_branch = detect_default_branch(dir)?;

    let output = Command::new("git")
        .args(["merge-base", "HEAD", &default_branch])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if base.is_empty() { None } else { Some(base) }
}

/// デフォルトブランチの参照名を検出する。優先順位:
///   1. `origin/HEAD` が指すブランチ
///   2. `origin/main`, `origin/master`
///   3. ローカル `main`, `master`
fn detect_default_branch(dir: &str) -> Option<String> {
    // 1. origin/HEAD symbolic ref
    if let Ok(output) = Command::new("git")
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .current_dir(dir)
        .output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }

    // 2 & 3. 候補を順に試す
    for candidate in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        let output = Command::new("git")
            .args(["rev-parse", "--quiet", "--verify", candidate])
            .current_dir(dir)
            .output();
        if let Ok(out) = output
            && out.status.success()
        {
            // rev-parse が成功したら、ブランチ名を返す
            let short_name = candidate
                .strip_prefix("refs/remotes/")
                .or_else(|| candidate.strip_prefix("refs/heads/"))
                .unwrap_or(candidate)
                .to_string();
            return Some(short_name);
        }
    }

    None
}

/// blame ベースの共変更解析。
///
/// 起点ファイル `source_files` の **変更行** に対して `git blame -L` を当て、
/// 最終修正コミット集合 `C` を作る。各 c ∈ C の `git diff-tree --name-only -r c`
/// から起点以外の共起ファイルを集計し、`co_changes / |C|` を confidence とする。
///
/// 旧 lookback ベースより文脈依存性が高く (ユーザーが今修正中のコードに密結合な
/// ペアだけが浮上)、大規模リポでも履歴全体を舐めずに済む。
pub fn analyze_cochange_blame(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    if !opts.blame {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            "analyze_cochange_blame called without blame mode enabled".to_string(),
        ));
    }
    if opts.source_files.is_empty() {
        return Ok(CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
        });
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

    let base_rev: &str = opts.base.as_deref().unwrap_or("HEAD~1");

    // Phase 1: 起点ファイルごとに blame で base コミット側の変更行 SHA を集める。
    //          ファイル単位で rayon 並列化する。
    check_timeout("phase1_blame_setup")?;
    let blame_per_file: Vec<HashSet<String>> = opts
        .source_files
        .par_iter()
        .map(|f| {
            collect_blame_commits_for_file(dir, f, base_rev, opts.rename, opts.copy)
                .unwrap_or_default()
        })
        .collect();
    check_timeout("phase1_blame_collected")?;

    let mut commit_set: HashSet<String> = HashSet::new();
    for s in &blame_per_file {
        commit_set.extend(s.iter().cloned());
    }
    if commit_set.is_empty() {
        return Ok(CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
        });
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
            return Ok(CoChangeResult {
                entries: Vec::new(),
                commits_analyzed: 0,
            });
        }
    }
    // blame 結果からも同 SHA を除外して per-source 集計と整合させる。
    let blame_per_file: Vec<HashSet<String>> = if opts.ignore_merges {
        blame_per_file
            .into_iter()
            .map(|s| {
                s.into_iter()
                    .filter(|sha| commit_set.contains(sha))
                    .collect()
            })
            .collect()
    } else {
        blame_per_file
    };
    check_timeout("phase3_diff_tree_setup")?;

    // Phase 2: 各コミット c の変更ファイルを diff-tree で取得 (並列化)。
    //          max_files_per_commit を超えるコミット (squash / 大量生成) はスキップ。
    let commits: Vec<String> = commit_set.into_iter().collect();
    let denominator = commits.len();
    let commit_files: Vec<Vec<String>> = commits
        .par_iter()
        .map(|sha| {
            let files = collect_files_in_commit(dir, sha).unwrap_or_default();
            if files.len() > opts.max_files_per_commit {
                Vec::new()
            } else {
                files
            }
        })
        .collect();

    // Phase 3: 候補ファイルごとの共起カウント。
    //          起点ファイル自身は候補から除外。
    //          除外 glob は既定 + 利用者指定 を結合して適用。
    let source_set: HashSet<&str> = opts.source_files.iter().map(String::as_str).collect();
    let exclude_matcher = build_exclude_matcher(&opts.exclude_globs)?;

    let mut co_counts: HashMap<String, usize> = HashMap::new();
    for files in &commit_files {
        // 同一コミットで g が複数ファイルに重複登録されないよう dedup
        let mut seen: HashSet<&str> = HashSet::new();
        for f in files {
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
            *co_counts.entry(path.to_string()).or_insert(0) += 1;
        }
    }

    // Phase 4: 起点ファイルごとに該当候補のスコアを集計し entries を組む。
    //          詳細は `aggregate_source_entries` を参照。
    let smoothing_on = !opts.disable_smoothing;
    let min_denom = opts.min_denominator.max(1); // 0 は 1 と同義 (= 既存挙動)

    let ctx = AggregationContext {
        commits: &commits,
        commit_files: &commit_files,
        source_set: &source_set,
        exclude_matcher: &exclude_matcher,
        co_counts: &co_counts,
        opts,
        smoothing_on,
    };
    let mut entries: Vec<CoChangeEntry> = Vec::new();
    for (i, source) in opts.source_files.iter().enumerate() {
        let blame_set = &blame_per_file[i];
        if blame_set.len() < min_denom {
            continue;
        }
        entries.extend(aggregate_source_entries(source, blame_set, &ctx));
    }

    // 全体 ranking。smoothing 有効なら score 降順、無効なら confidence 降順。
    entries.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));

    Ok(CoChangeResult {
        entries,
        commits_analyzed: denominator,
    })
}

/// `aggregate_source_entries` に渡す共通コンテキスト。
/// 起点ファイルごとに変わらない参照群をまとめて clippy::too_many_arguments を回避する。
struct AggregationContext<'a> {
    commits: &'a [String],
    commit_files: &'a [Vec<String>],
    source_set: &'a HashSet<&'a str>,
    exclude_matcher: &'a CoChangeExclude,
    co_counts: &'a HashMap<String, usize>,
    opts: &'a CoChangeOptions,
    smoothing_on: bool,
}

/// 1 起点ファイルに紐づく blame コミット集合から、共起候補ごとの CoChangeEntry を集計する。
///
/// 適用フィルタ:
///   * `min_samples`: co_changes 未満の候補は除外
///   * `min_confidence`: ranking 値 (smoothing on/off で切替) が閾値未満は除外
///   * `per_source_limit`: ranking 値降順で上位 N 件に truncate (0 = 無制限)
///
/// 集計内訳:
///   * `confidence` = co / local_denom (raw)
///   * `score`      = (co + α) / (local_denom + α + β) (smoothing on) または confidence (off)
fn aggregate_source_entries(
    source: &str,
    blame_set: &HashSet<String>,
    ctx: &AggregationContext<'_>,
) -> Vec<CoChangeEntry> {
    // 起点 blame 集合内で各候補の共起回数を再集計
    let mut per_source: HashMap<&str, usize> = HashMap::new();
    for (j, sha) in ctx.commits.iter().enumerate() {
        if !blame_set.contains(sha) {
            continue;
        }
        let files = &ctx.commit_files[j];
        let mut seen: HashSet<&str> = HashSet::new();
        for f in files {
            let path = f.as_str();
            if !seen.insert(path) {
                continue;
            }
            if ctx.source_set.contains(path) {
                continue;
            }
            if ctx.exclude_matcher.is_match(path) {
                continue;
            }
            *per_source.entry(path).or_insert(0) += 1;
        }
    }

    let local_denom = blame_set.len() as f64;
    let alpha = ctx.opts.smoothing_alpha;
    let beta = ctx.opts.smoothing_beta;

    let mut per_source_entries: Vec<CoChangeEntry> = Vec::new();
    for (cand, co) in per_source {
        if co < ctx.opts.min_samples {
            continue;
        }
        let confidence = co as f64 / local_denom;
        let score = if ctx.smoothing_on {
            (co as f64 + alpha) / (local_denom + alpha + beta)
        } else {
            confidence
        };
        let entry = CoChangeEntry {
            file_a: source.to_string(),
            file_b: cand.to_string(),
            co_changes: co,
            total_changes_a: blame_set.len(),
            total_changes_b: *ctx.co_counts.get(cand).unwrap_or(&0),
            confidence,
            denominator: Some(blame_set.len()),
            score: Some(score),
        };
        // ranking 値で min_confidence を判定 (smoothing on/off で一貫)
        if entry.ranking_value(ctx.smoothing_on) < ctx.opts.min_confidence {
            continue;
        }
        per_source_entries.push(entry);
    }

    per_source_entries.sort_by(|a, b| compare_entries_by_ranking(a, b, ctx.smoothing_on));
    if ctx.opts.per_source_limit > 0 {
        per_source_entries.truncate(ctx.opts.per_source_limit);
    }
    per_source_entries
}

/// CoChangeEntry を ranking 値で降順比較する。同値時は path 昇順で安定化する。
fn compare_entries_by_ranking(
    a: &CoChangeEntry,
    b: &CoChangeEntry,
    smoothing_on: bool,
) -> std::cmp::Ordering {
    b.ranking_value(smoothing_on)
        .partial_cmp(&a.ranking_value(smoothing_on))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.file_a.cmp(&b.file_a))
        .then_with(|| a.file_b.cmp(&b.file_b))
}

/// 1 ファイルの diff hunk 群を 1 回の `git blame -L S,+C [-L S,+C]...` にまとめて
/// 最終修正コミット SHA 集合を取得する。
/// - 純粋追加 hunk (old_count = 0) は blame 不要なのでスキップする。
/// - 全 hunk が純粋追加だった場合は空集合を返す。
/// - blame の失敗 (binary / non-existent in base) は空集合を返して継続。
fn collect_blame_commits_for_file(
    dir: &str,
    file: &str,
    base: &str,
    rename: bool,
    copy: bool,
) -> Result<HashSet<String>> {
    // diff --unified=0 で hunk header を取得
    let diff_output = Command::new("git")
        .args(["diff", "--unified=0", base, "HEAD", "--", file])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !diff_output.status.success() {
        return Ok(HashSet::new());
    }
    let diff_text = String::from_utf8_lossy(&diff_output.stdout);

    // hunk 旧側 (start, count) を抽出
    let ranges: Vec<(u64, u64)> = parse_hunk_old_ranges(&diff_text);
    if ranges.is_empty() {
        return Ok(HashSet::new());
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
    for (start, count) in &ranges {
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
        return Ok(HashSet::new());
    }
    let blame_text = String::from_utf8_lossy(&blame_output.stdout);

    let mut shas: HashSet<String> = HashSet::new();
    for line in blame_text.lines() {
        // line-porcelain: 各エントリの先頭行は `<sha40> <orig_line> <final_line> [count]`。
        // メタデータ行 (author / summary / filename) は日本語等のマルチバイト文字を含む
        // ことがあるため、byte レベルで先頭 40 バイトが hex + 41 番目がスペースか確認する
        // (str スライスを byte 境界で切るとパニックするため)。
        let bytes = line.as_bytes();
        if bytes.len() < 41 || bytes[40] != b' ' {
            continue;
        }
        let sha_bytes = &bytes[..40];
        if !sha_bytes.iter().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // 安全: ASCII hex のみで構成されているので UTF-8 検証は不要。
        let sha = std::str::from_utf8(sha_bytes).expect("ascii hex is utf-8");
        shas.insert(sha.to_string());
    }
    Ok(shas)
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

/// `git ls-tree -r --name-only HEAD` で HEAD ツリーのファイル一覧を取得する。
fn list_head_tree(dir: &str) -> Result<HashSet<String>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git ls-tree failed: {stderr}"),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
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

    fn rm_commit(repo: &std::path::Path, file: &str, msg: &str) {
        std::process::Command::new("git")
            .args(["rm", file])
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
        let mut o = CoChangeOptions::default();
        // テストは merge-base 打ち切り/skip deleted をデフォルト有効のまま扱う
        // 必要なテストだけ mutator で切り替える
        mutator(&mut o);
        o
    }

    /// git 未初期化のディレクトリでエラーを返す
    #[test]
    fn analyze_cochange_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = analyze_cochange(
            dir.path().to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        );
        assert!(result.is_err());
    }

    /// 空リポジトリでもパニックしない
    #[test]
    fn analyze_cochange_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // コミットがないリポジトリでは git log がエラーを返す
        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        );
        assert!(result.is_err());
    }

    /// 3 回同時変更 → 正常に検出される
    #[test]
    fn analyze_cochange_detects_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}")),
                    ("b.rs", &format!("fn b() {{ {i} }}")),
                ],
                &format!("pair {i}"),
            );
        }

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert_eq!(result.commits_analyzed, 3);
        assert!(!result.entries.is_empty());
        assert!(result.entries.iter().any(|e| {
            (e.file_a == "a.rs" && e.file_b == "b.rs") || (e.file_a == "b.rs" && e.file_b == "a.rs")
        }));
    }

    /// filter_file で特定ファイルのペアのみ返す
    #[test]
    fn analyze_cochange_filter_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("// {i}")),
                    ("b.rs", &format!("// {i}")),
                    ("c.rs", &format!("// {i}")),
                ],
                &format!("triple {i}"),
            );
        }

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
                o.filter_file = Some("a.rs".into());
            }),
        )
        .unwrap();
        for entry in &result.entries {
            assert!(entry.file_a == "a.rs" || entry.file_b == "a.rs");
        }
    }

    /// max_files_per_commit を超える初期 bulk コミットはスキップされる
    #[test]
    fn analyze_cochange_skips_bulk_initial_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 31 ファイルを 1 コミットで追加（閾値 30 を超える）
        let names: Vec<String> = (0..31).map(|i| format!("file_{i}.rs")).collect();
        let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "// init")).collect();
        commit_files(repo, &files, "bulk initial commit");

        // 個別ファイルを更新
        commit_files(repo, &[("file_0.rs", "// updated")], "update file_0");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 50;
                o.min_confidence = 0.0;
                o.min_samples = 1;
                o.max_files_per_commit = 30;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert!(
            result.entries.is_empty(),
            "bulk commit should not generate cochange pairs, got: {:?}",
            result.entries
        );
    }

    /// min_samples 未満のペアは発報されない
    #[test]
    fn analyze_cochange_requires_minimum_samples() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // a.rs と b.rs を 2 回だけ同時変更
        for i in 0..2 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("pair {i}"),
            );
        }
        // a.rs を単独で変更
        commit_files(repo, &[("a.rs", "// solo")], "solo");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.0;
                o.min_samples = 3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert!(
            result.entries.is_empty(),
            "co_changes=2 should be filtered when min_samples=3, got: {:?}",
            result.entries
        );
    }

    /// HEAD に存在しないファイルを含むペアはスキップされる
    #[test]
    fn analyze_cochange_skips_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // a.rs と b.rs を 3 回同時変更（閾値を満たす）
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("pair {i}"),
            );
        }
        // b.rs を削除
        rm_commit(repo, "b.rs", "remove b.rs");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.0;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = true;
            }),
        )
        .unwrap();
        assert!(
            result
                .entries
                .iter()
                .all(|e| e.file_a != "b.rs" && e.file_b != "b.rs"),
            "deleted file should not appear in cochange entries, got: {:?}",
            result.entries
        );
    }

    /// merge-base 打ち切りで feature branch のコミットが統計に混ざらない
    #[test]
    fn analyze_cochange_bounded_by_merge_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // main 上で x.rs と y.rs を 4 回同時変更（真の cochange ペア）
        for i in 0..4 {
            commit_files(
                repo,
                &[("x.rs", &format!("// {i}")), ("y.rs", &format!("// {i}"))],
                &format!("main pair {i}"),
            );
        }

        // feature ブランチを切って a.rs と b.rs を 3 回同時変更
        std::process::Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(repo)
            .output()
            .unwrap();
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("feature pair {i}"),
            );
        }

        // merge-base 打ち切り有効で解析する
        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 50;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = true;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();

        // feature 内の a.rs↔b.rs ペアは含まれないはず
        assert!(
            result
                .entries
                .iter()
                .all(|e| !((e.file_a == "a.rs" && e.file_b == "b.rs")
                    || (e.file_a == "b.rs" && e.file_b == "a.rs"))),
            "feature branch pair should not appear, got: {:?}",
            result.entries
        );
        // main 側の x.rs↔y.rs は検出される
        assert!(
            result
                .entries
                .iter()
                .any(|e| (e.file_a == "x.rs" && e.file_b == "y.rs")
                    || (e.file_a == "y.rs" && e.file_b == "x.rs")),
            "main pair should still be detected, got: {:?}",
            result.entries
        );
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();

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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string(), "b.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();

        // 起点が a.rs / b.rs で、両者ともお互いを候補にしてはならない
        for e in &result.entries {
            assert!(
                !(e.file_b == "a.rs" || e.file_b == "b.rs"),
                "source file appeared as candidate: {e:?}"
            );
        }
    }

    /// 純粋追加 hunk (旧 count=0) しかないファイルは blame 対象がなく、
    /// 起点に紐づく blame コミット集合は空になり entries も空になる。
    #[test]
    fn analyze_cochange_blame_pure_addition_yields_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 何かしら 1 コミット作って HEAD~1 を解決可能にする
        commit_files(repo, &[("seed.rs", "// seed")], "seed");
        // 新規ファイル a.rs を追加 (= base に存在しない、純粋追加)
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "add a");

        let opts = opts_with(|o| {
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty() && result.commits_analyzed == 0,
            "pure-addition source should yield empty result, got: {result:?}"
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            // 3 件起点 / 上限 2 → reject
            o.source_files = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
            o.max_source_files = 2;
            o.base = Some("HEAD".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
        });
        let err = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap_err();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.max_source_files = 0; // unlimited
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        // エラーにならず Ok で返ること (entries の中身は問わない)
        let _ = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["new.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        // rename=true: old.rs の旧履歴を辿れて b.rs と共起検出されるはず
        opts.rename = true;
        let with_rename = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
            o.ignore_merges = false;
        });

        // 比較: ignore_merges=false / true で commits_analyzed が増減すること
        let baseline = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
        opts.ignore_merges = true;
        let filtered = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.max_blame_commits = 1; // SHA 集合 3 件 > 上限 1 で停止
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let err = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap_err();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~3".to_string());
            o.max_blame_commits = 0; // unlimited
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        // エラー無く Ok で返ること
        let _ = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.timeout_secs = 60; // 十分大きな上限
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let _ = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.timeout_secs = 0; // unlimited
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let _ = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["copy.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.copy = true; // -C 有効化
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        // エラーなく完走することを最低限確認する
        // (-C 検出の再現性はテスト環境/git バージョン依存があるため、
        //  ここでは「-C を渡しても crash しない」ことだけ保証する)
        let _ = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.smoothing_alpha = 1.0;
            o.smoothing_beta = 4.0;
            o.disable_smoothing = false;
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.disable_smoothing = true;
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_denominator = 2; // 1 件しかない起点は除外
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.per_source_limit = 1; // 候補 1 件まで
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
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
            o.blame = true;
            o.source_files = vec!["a".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.per_source_limit = 0;
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
        let from_a: Vec<_> = result.entries.iter().filter(|e| e.file_a == "a").collect();
        assert!(
            from_a.len() >= 2,
            "per_source_limit=0 should keep multiple candidates, got: {:?}",
            from_a,
        );
    }

    /// lookback モードは `score` フィールドを出さない (互換維持)。
    #[test]
    fn analyze_cochange_lookback_does_not_emit_score() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a", &format!("fn a() {{ {i} }}\n")),
                    ("b", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("p{i}"),
            );
        }
        let opts = opts_with(|o| {
            o.lookback = 10;
            o.min_confidence = 0.0;
            o.min_samples = 2;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        for e in &result.entries {
            assert!(
                e.score.is_none(),
                "lookback entries must not have score, got: {e:?}"
            );
        }
    }
}
