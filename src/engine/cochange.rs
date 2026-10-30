use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::error::{AstroError, ErrorCode};
use crate::git_support::{DEFAULT_BLAME_BASE, validate_git_revision};
use crate::models::cochange::{
    BLAME_DEFAULT_EXCLUDE_GLOBS, CoChangeDiagnosticReason, CoChangeDiagnostics, CoChangeEntry,
    CoChangeEvidence, CoChangeOptions, CoChangeResult,
};
use crate::models::skip::SkipInfo;

mod generated_policy;
mod scoring;
use generated_policy::GeneratedPolicy;
use scoring::{ShardStats, commit_size_weight, compare_entries_by_ranking};

/// 起点ファイルを pathspec として渡す git 呼び出しに付ける環境変数。
///
/// 付けないと `:/secret/x.rs` や `:(top)secret/x.rs` (pathspec magic) がリポジトリの
/// ルートから解決され、session / MCP のワークスペース外にあるファイルの履歴を
/// 解析できてしまう (`..` は字句検証で拒否しているのに、magic は素通りしていた)。
/// 文字どおりに解釈させれば、`:` で始まる実在のファイル名も壊さずに扱える。
const LITERAL_PATHSPECS_ENV: &str = "GIT_LITERAL_PATHSPECS";

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

/// 解析全体のタイムアウト判定 (`timeout_secs = 0` で無制限)。
///
/// 各フェーズ入口で `check` を呼ぶ。既に走行中の subprocess (git blame / diff-tree) は
/// kill しないため、直近の 1 起動の完了までは待つ実装 (実用上の許容範囲)。
struct Deadline {
    started: std::time::Instant,
    timeout_secs: u64,
}

impl Deadline {
    fn new(timeout_secs: u64) -> Self {
        Self {
            started: std::time::Instant::now(),
            timeout_secs,
        }
    }

    /// 超過していれば停止する。`phase` はどこで超過したかの申告用。
    fn check(&self, phase: &str) -> Result<()> {
        if self.timeout_secs == 0 {
            return Ok(());
        }
        let elapsed = self.started.elapsed().as_secs();
        if elapsed >= self.timeout_secs {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "blame analysis exceeded --timeout-secs {} during {phase} (elapsed {elapsed}s)",
                    self.timeout_secs,
                ),
            ));
        }
        Ok(())
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
///
/// # 入力の前提
///
/// `opts.source_files` は **`--dir` 基準に正規化済みの相対パス** (`./` や連続スラッシュを
/// 含まない `/` 区切り) であること。起点の自己除外は生の文字列一致で行う一方、候補側は
/// `git diff-tree --name-only` 由来の正規形なので、未正規化のパスを渡すと**起点自身が
/// 共変更相手として返る**。
///
/// 製品内経路 (CLI / MCP / session / review) は必ず [`crate::service::AppService`] の
/// `analyze_cochange` を通り、そこで検証と正規化がまとめて行われる。ライブラリとして
/// この関数を直接呼ぶ場合は呼び出し側で正規化すること。
pub fn analyze_cochange(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    if opts.source_files.is_empty() {
        return Ok(empty_cochange_result(None, CoChangeDiagnostics::default()));
    }
    // 起点ファイル数の上限ガード (0 = 無制限)。暴走防止のため超過は明示的に停止する。
    // AppService 側でも正規化後の unique 件数に対して先に掛かるので、ここは engine を
    // 直接呼ぶ利用者向けの多層防御。
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

    let deadline = Deadline::new(opts.timeout_secs);

    let base_rev: &str = opts.base.as_deref().unwrap_or(DEFAULT_BLAME_BASE);
    // base は git diff / git blame に直接渡されるため、`-` プレフィクス等の
    // オプション誤認識を防ぐ revision 検証を必ず通す。--paths/--paths-file 経由で
    // resolve_blame_source_files の検証を迂回しても安全側に倒れる。
    validate_git_revision(base_rev, "--base")?;

    let mut diag = CoChangeDiagnostics {
        sources_requested: opts.source_files.len(),
        ..Default::default()
    };
    let min_denom = opts.min_denominator.max(1);

    let index = build_evidence_index(dir, base_rev, min_denom, opts, &deadline, &mut diag)?;
    if index.is_empty() {
        return Ok(empty_cochange_result(None, diag));
    }
    // 証拠として集めたユニークコミット集合 |C| のサイズ。集計に入る前の値なので、
    // 後段で diff-tree に失敗したコミットもここには含まれる (下の commit_scan_failures 参照)。
    let commits_analyzed = index.sha_to_sources.len();

    deadline.check("phase3_diff_tree_setup")?;
    let stats = aggregate_commits(dir, &index, opts)?;
    if stats.failed_commits > 0 {
        // git 失敗を「変更ファイル 0 件」に畳んだまま黙っていると、「解析できなかった」が
        // 「共変更が無かった」と区別できなくなる (0 件の理由を返すという診断の趣旨に反する)。
        //
        // `commits_analyzed` からは差し引かない。これは「証拠として集めたユニークコミット
        // 集合 |C| のサイズ」という別の量で、実際に集計対象となった件数は per-source の
        // `entry.denominator` が持つ。失敗分を引くと 3 つ目の意味を持つ数字になり、
        // 既存出力の意味も変わってしまうため、失敗件数は別フィールドで申告する。
        // なお失敗コミットは「変更 0 件のコミット」として実効分母には入る (挙動は据え置き)。
        diag.commit_scan_failures = stats.failed_commits;
        diag.add_reason(CoChangeDiagnosticReason::CommitScanFailed);
    }

    deadline.check("phase4_assemble")?;

    // base リビジョンの棚卸しを 1 回だけ行い、削除済みパスの候補を落とす。
    // 失敗したら None = フィルタ無効 (従来どおりの出力) に倒す。
    let base_tree_paths = collect_base_tree_paths(dir, base_rev);
    let candidate_policy = resolve_candidate_generated_policy(dir, opts, &stats);
    if candidate_policy.attr_lookup_failed() {
        diag.add_reason(CoChangeDiagnosticReason::GeneratedAttrLookupFailed);
    }
    let filters = CandidateFilters {
        base_tree_paths: base_tree_paths.as_ref(),
        generated: &candidate_policy,
    };
    let entries = assemble_entries(opts, &index, &stats, min_denom, &filters, &mut diag);
    diag.finalize();

    Ok(CoChangeResult {
        entries,
        commits_analyzed,
        diagnostics: diag,
        skipped: None,
    })
}

/// 起点別の証拠コミット集合と、その SHA 逆引き index。
///
/// `per_source[i]` は `opts.source_files[i]` に対応する (入力順を保持する)。
/// `sha_to_sources` は「この SHA を証拠に持つ起点は誰か」の逆引きで、diff-tree の結果を
/// 全保持せずに SHA 1 件ずつ per-source 集計へ畳み込むために使う。
struct EvidenceIndex {
    per_source: Vec<SourceEvidence>,
    sha_to_sources: HashMap<String, Vec<usize>>,
    sha_to_info: HashMap<String, BlameInfo>,
}

impl EvidenceIndex {
    /// 集計対象のコミットが 1 件も無い (証拠を作れなかった / マージ除外で全滅した)。
    fn is_empty(&self) -> bool {
        self.sha_to_sources.is_empty()
    }
}

/// 起点ごとに証拠コミット集合を作り、SHA 逆引き index に畳む。
///
/// 1. 起点ファイル単位で rayon 並列に blame (不足時は履歴 fallback) を実行する
/// 2. 証拠の内訳を診断へ反映する
/// 3. 上限ガードとマージ除外で集計対象 SHA を確定する
/// 4. SHA → 起点 index / BlameInfo の逆引きを作る
///
/// 集計対象が 0 件のときは空の index を返す (呼び出し側が空結果へ倒す)。
fn build_evidence_index(
    dir: &str,
    base: &str,
    min_denom: usize,
    opts: &CoChangeOptions,
    deadline: &Deadline,
    diag: &mut CoChangeDiagnostics,
) -> Result<EvidenceIndex> {
    deadline.check("phase1_blame_setup")?;
    // 生成物と宣言された起点は証拠収集そのものを行わない。バッチが書き出したファイルの
    // 履歴を辿っても、得られるのは「同じバッチが同時に書いた相手」だけで、人手の変更に
    // 対する共変更候補にならない。`source_files` との添字対応は集計全体の前提なので、
    // 起点を配列から取り除かず「証拠なし」の印を付けて位置を保つ。
    let source_policy = if opts.include_generated {
        GeneratedPolicy::disabled()
    } else {
        GeneratedPolicy::resolve(dir, opts.source_files.iter().map(String::as_str))
    };
    if source_policy.attr_lookup_failed() {
        diag.add_reason(CoChangeDiagnosticReason::GeneratedAttrLookupFailed);
    }
    // 変更行は起点ごとに `git diff` を起動せず、まとめて取る (取れなかった起点だけ 1 件ずつ)。
    let evidence_sources: Vec<&str> = opts
        .source_files
        .iter()
        .map(String::as_str)
        .filter(|f| source_policy.excluded(f).is_none())
        .collect();
    let batched_changes = collect_changed_old_ranges_batch(
        dir,
        &evidence_sources,
        base,
        opts.changed_lines_from_index,
    );
    let mut per_source: Vec<SourceEvidence> = opts
        .source_files
        .par_iter()
        .map(|f| {
            if source_policy.excluded(f).is_some() {
                return SourceEvidence::generated_excluded();
            }
            collect_evidence_for_file(
                dir,
                f,
                base,
                min_denom,
                opts,
                batched_changes.get(f.as_str()),
            )
        })
        .collect();
    deadline.check("phase1_blame_collected")?;
    record_evidence_diagnostics(&per_source, diag);

    let commit_set = resolve_target_commits(dir, &mut per_source, opts, deadline)?;
    if commit_set.is_empty() {
        return Ok(EvidenceIndex {
            per_source,
            sha_to_sources: HashMap::new(),
            sha_to_info: HashMap::new(),
        });
    }

    let mut sha_to_sources: HashMap<String, Vec<usize>> = HashMap::new();
    let mut sha_to_info: HashMap<String, BlameInfo> = HashMap::new();
    for (i, ev) in per_source.iter().enumerate() {
        for (sha, info) in &ev.commits {
            sha_to_sources.entry(sha.clone()).or_default().push(i);
            // 起点間で同じ SHA の info があれば最初に見たものを採用する
            sha_to_info
                .entry(sha.clone())
                .or_insert_with(|| info.clone());
        }
    }
    Ok(EvidenceIndex {
        per_source,
        sha_to_sources,
        sha_to_info,
    })
}

/// 証拠収集の結果を診断へ反映する。
///
/// 「共変更が無い」のか「証拠を作れなかった」のかを呼び出し側が区別できるよう、
/// 起点ごとの内訳 (git 失敗 / 変更行なし / 新規ファイル / blame・履歴の別) を残す。
fn record_evidence_diagnostics(per_source: &[SourceEvidence], diag: &mut CoChangeDiagnostics) {
    for ev in per_source {
        if ev.generated_excluded {
            // 「証拠を作れなかった」ではなく「作らないと決めた」ので、
            // sources_without_evidence には数えない。
            diag.excluded_generated_sources += 1;
            diag.add_reason(CoChangeDiagnosticReason::SourceIsGenerated);
            continue;
        }
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
}

/// 全起点の証拠 SHA の和集合から、集計対象のコミット集合を確定する。
///
/// 上限ガード (`max_blame_commits`) を掛け、`ignore_merges` ならマージコミットを除外する。
/// 除外した SHA は `per_source` 側の証拠集合からも落とし、per-source 集計と整合させる。
/// 戻り値が空なら集計対象のコミットが 1 件も無い。
fn resolve_target_commits(
    dir: &str,
    per_source: &mut [SourceEvidence],
    opts: &CoChangeOptions,
    deadline: &Deadline,
) -> Result<HashSet<String>> {
    let mut commit_set: HashSet<String> = HashSet::new();
    for ev in per_source.iter() {
        for k in ev.commits.keys() {
            commit_set.insert(k.clone());
        }
    }
    if commit_set.is_empty() {
        return Ok(commit_set);
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
    if !opts.ignore_merges {
        return Ok(commit_set);
    }

    // git rev-list --no-walk --merges <SHA>... は引数の SHA のうちマージのみを返すので、
    // 一括問い合わせで効率良く判定できる (ARG_MAX 超過対策で chunk 化)。
    deadline.check("phase2_merge_filter")?;
    let merge_set = list_merge_commits(dir, &commit_set)?;
    commit_set.retain(|s| !merge_set.contains(s));
    for ev in per_source.iter_mut() {
        ev.commits.retain(|sha, _| commit_set.contains(sha));
    }
    Ok(commit_set)
}

/// コミットを跨いで不変な集計入力。fold の各シャードから共有参照で読む。
struct CommitScanContext<'a> {
    dir: &'a str,
    /// リポジトリルート相対のパスを `dir` 相対へ直す prefix (`dir` がルートなら空)。
    workspace_prefix: String,
    opts: &'a CoChangeOptions,
    /// 起点自身は共変更相手にしない。
    source_set: HashSet<&'a str>,
    exclude: CoChangeExclude,
    sha_to_info: &'a HashMap<String, BlameInfo>,
}

impl CommitScanContext<'_> {
    /// SHA を「(author_mail, time_bucket)」の knowledge unit に圧縮したキー。
    ///
    /// 同一 author の連続 commit を 1 unit として扱い、短時間 burst による偽陽性を抑える。
    /// `author_unit_window_days = 0` (旧挙動) では常に None で、raw weighted 集計のみが有効。
    fn unit_key(&self, sha: &str) -> Option<(String, i64)> {
        let window_days = self.opts.author_unit_window_days;
        if window_days == 0 {
            return None;
        }
        let info = self.sha_to_info.get(sha)?;
        let mail = info.author_mail.as_ref()?;
        let time = info.author_time?;
        let bucket_seconds = (window_days as i64) * 86_400;
        Some((mail.clone(), time / bucket_seconds))
    }

    /// 共変更候補として数えるパスか (起点自身と除外 glob を落とす)。
    fn is_candidate(&self, path: &str) -> bool {
        !self.source_set.contains(path) && !self.exclude.is_match(path)
    }
}

/// 証拠コミットの変更ファイルを取り、per-source 集計へ畳み込む。
///
/// 変更ファイルはコミットごとに diff-tree を起動せず、[`DIFF_TREE_BATCH_COMMITS`] 件ずつ
/// 1 プロセスの `git diff-tree --stdin` で取る ([`scan_commit_chunk`])。まとまりは rayon で
/// 並列に処理し、各コミットの結果はその場でサイズ重み計算 → per-source 集計 (raw / weighted /
/// units) と global co_counts に畳み込む。`commit_files: Vec<Vec<String>>` を全保持しないため、
/// 大規模 diff (commits 数百〜千超) でもピーク RSS が線形以下に収まる。集計は件数の
/// ヒストグラムなので、まとまりの分け方や畳む順序に結果は依存しない。
fn aggregate_commits(
    dir: &str,
    index: &EvidenceIndex,
    opts: &CoChangeOptions,
) -> Result<ShardStats> {
    let exclude = build_exclude_matcher(&opts.exclude_globs)?;
    // SHA ごとに `git rev-parse` を起動しないよう、ワークスペース prefix は 1 度だけ解決する。
    let workspace_prefix = workspace_prefix_for(dir)?;
    let ctx = CommitScanContext {
        dir,
        workspace_prefix,
        opts,
        source_set: opts.source_files.iter().map(String::as_str).collect(),
        exclude,
        sha_to_info: &index.sha_to_info,
    };

    let n_sources = opts.source_files.len();
    let sha_entries: Vec<(&String, &Vec<usize>)> = index.sha_to_sources.iter().collect();
    Ok(sha_entries
        .par_chunks(DIFF_TREE_BATCH_COMMITS)
        .map(|chunk| scan_commit_chunk(&ctx, chunk, n_sources))
        .reduce(|| ShardStats::new(n_sources), ShardStats::merge))
}

/// 1 つの `git diff-tree --stdin` に流すコミット数の上限。
///
/// 1 コミットあたりの処理は 1ms 未満で、Windows では起動の数十 ms の方が重い (実測: 1 件ずつだと
/// 48 コミットで 2.3 秒、まとめると 0.1 秒)。通常の review の証拠コミットはこれ以下なので
/// 1 プロセスで済み、上回る大きな差分だけを複数のプロセスに分けて並列に流す。
const DIFF_TREE_BATCH_COMMITS: usize = 512;

/// `git diff-tree --stdin` でコミットの区切りとして書く行。
///
/// diff-tree は stdin の行のうち OID として読めないものを、そのまま stdout へ書き戻して flush する
/// (builtin/diff-tree.c)。`--name-only` が出すパスは `/` で始まらず、改行などの特殊文字を含む
/// パスは引用されるので、この行がパスの行と取り違えられることはない。
const DIFF_TREE_SEPARATOR: &[u8] = b"/astro-sight-commit-end";

/// 1 まとまりの証拠コミットの変更ファイルを取り、集計する。
///
/// まず 1 プロセスの `git diff-tree --stdin` でまとめて取る ([`run_diff_tree_batch`])。
/// プロセスが正常に終わらなかった (起動・書き込みの失敗、非 0 終了、出力が想定の形でない、途中で
/// 終わった) ときは、このまとまりの仮集計を捨てて全コミットを 1 件ずつ取り直す。正常に終わり、
/// 特定のコミットの見出しだけが無い (diff-tree がそのコミットを読めなかった) ときは、そのコミット
/// だけを 1 件ずつ取る。どちらも失敗の判定と件数 (`failed_commits`) は 1 件ずつの経路に任せるので、
/// 結果はコミットごとに `git diff-tree` を起動した場合と同じになる。
fn scan_commit_chunk(
    ctx: &CommitScanContext<'_>,
    chunk: &[(&String, &Vec<usize>)],
    n_sources: usize,
) -> ShardStats {
    let mut acc = ShardStats::new(n_sources);
    let shas: Vec<&str> = chunk.iter().map(|(sha, _)| sha.as_str()).collect();
    let completed = run_diff_tree_batch(ctx.dir, &shas, &mut |i, lines| {
        let (sha, sources) = chunk[i];
        let commit =
            commit_files_from_lines(lines.iter().map(String::as_str), &ctx.workspace_prefix);
        accumulate_commit(&mut acc, ctx, sha, sources, Ok(commit));
    });
    let retry: Vec<usize> = match completed {
        Some(completed) => (0..chunk.len()).filter(|&i| !completed[i]).collect(),
        None => {
            acc = ShardStats::new(n_sources);
            (0..chunk.len()).collect()
        }
    };
    let retried = retry
        .par_iter()
        .fold(
            || ShardStats::new(n_sources),
            |mut acc, &i| {
                let (sha, sources) = chunk[i];
                let commit = collect_files_in_commit(ctx.dir, sha, &ctx.workspace_prefix);
                accumulate_commit(&mut acc, ctx, sha, sources, commit);
                acc
            },
        )
        .reduce(|| ShardStats::new(n_sources), ShardStats::merge);
    ShardStats::merge(acc, retried)
}

/// `shas` を 1 プロセスの `git diff-tree --stdin` に流し、見出しから区切りまで揃って読めたコミットの
/// 出力の行を `each(添字, 行)` に渡す。
///
/// stdin には `<sha>\n<区切り>\n` を順に書く。`--always` で差分が空のコミット (マージ・空コミット) も
/// 見出し (コミット ID) を出すので、見出しの無いまま区切りが来たコミットは diff-tree が読めなかった
/// ものと分かる。見出しと区切りの間の行は `git diff-tree --root --no-commit-id --name-only -r <sha>` の
/// 出力と同じ (`core.quotepath=off` と引用の扱いも同じにする)。
///
/// 戻り値は、プロセスが正常に終わり、全コミットの区切りまで想定どおりに読めたときだけ `Some`
/// (`completed[i]` = `each` に渡した)。それ以外は `None` で、`each` に渡した分は捨てる前提。
fn run_diff_tree_batch(
    dir: &str,
    shas: &[&str],
    each: &mut dyn FnMut(usize, &[String]),
) -> Option<Vec<bool>> {
    use std::io::{BufRead, Write};

    let mut child = Command::new("git")
        .args([
            "-c",
            "core.quotepath=off",
            "diff-tree",
            "--stdin",
            "--always",
            "--root",
            "--no-abbrev-commit",
            "--name-only",
            "-r",
        ])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let mut input = Vec::new();
    for sha in shas {
        input.extend_from_slice(sha.as_bytes());
        input.push(b'\n');
        input.extend_from_slice(DIFF_TREE_SEPARATOR);
        input.push(b'\n');
    }
    // 書き終えてから読むと、出力で pipe が埋まった git が書けずに止まり、こちらも書けずに止まる。
    let writer = std::thread::spawn(move || stdin.write_all(&input).is_ok());

    let mut completed = vec![false; shas.len()];
    let mut reached = 0;
    let mut intact = true;
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = Vec::new();
    let mut files: Option<Vec<String>> = None;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                intact = false;
                break;
            }
        }
        let (Some(text), Some(&sha)) = (line.strip_suffix(b"\n"), shas.get(reached)) else {
            intact = false;
            break;
        };
        match files.as_mut() {
            None if text == sha.as_bytes() => files = Some(Vec::new()),
            None if text == DIFF_TREE_SEPARATOR => reached += 1,
            Some(_) if text == DIFF_TREE_SEPARATOR => {
                each(reached, &files.take().unwrap_or_default());
                completed[reached] = true;
                reached += 1;
            }
            Some(lines) => lines.push(String::from_utf8_lossy(text).into_owned()),
            None => {
                intact = false;
                break;
            }
        }
    }
    // 途中で読むのをやめたときに git が書き込みで詰まらないよう、待つ前に pipe を閉じる。
    drop(reader);
    let written = writer.join().unwrap_or(false);
    let exited = child.wait().is_ok_and(|status| status.success());
    (intact && written && exited && reached == shas.len()).then_some(completed)
}

/// SHA 1 件分の変更ファイルを per-source 集計へ畳み込む。
///
/// `git diff-tree` の失敗は「変更ファイル 0 件」と同じ扱いで解析を続行しつつ、件数を
/// `failed_commits` へ持ち上げる。黙って 0 件に畳むと「解析できなかったコミット」が
/// 「共変更が無かったコミット」と区別できなくなるため (呼び出し側が診断で申告する)。
fn accumulate_commit(
    acc: &mut ShardStats,
    ctx: &CommitScanContext<'_>,
    sha: &str,
    sources: &[usize],
    commit: Result<CommitFiles>,
) {
    let commit = match commit {
        Ok(commit) => commit,
        Err(_) => {
            acc.failed_commits += 1;
            CommitFiles::default()
        }
    };
    // 規模判定はコミット全体のファイル数で行う (ワークスペース内の件数ではない)。
    let weight = commit_size_weight(
        commit.total_files,
        ctx.opts.commit_size_pivot,
        ctx.opts.max_files_per_commit,
    );
    if weight <= 0.0 {
        return;
    }
    let unit = ctx.unit_key(sha);
    for &i in sources {
        // confidence の分母は「実際に集計対象となったコミット数」。
        // weight 0 (max_files_per_commit 超過) のコミットは上で return 済みなので
        // ここに到達したものだけを数える。証拠集合サイズをそのまま分母にすると
        // co (超過コミットを含まない) と母数がずれ confidence が過小評価になる。
        acc.counted_denom[i] += 1;
        *acc.denom_weight_hist[i]
            .entry(commit.total_files)
            .or_insert(0) += 1;
        if let Some(u) = unit.as_ref() {
            acc.denom_units[i].insert(u.clone());
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for f in &commit.workspace_files {
        let path = f.as_str();
        if !seen.insert(path) {
            continue;
        }
        if !ctx.is_candidate(path) {
            continue;
        }
        *acc.co_counts.entry(path.to_string()).or_insert(0) += 1;
        for &i in sources {
            *acc.per_source_raw[i].entry(path.to_string()).or_insert(0) += 1;
            *acc.per_source_weight_hist[i]
                .entry(path.to_string())
                .or_default()
                .entry(commit.total_files)
                .or_insert(0) += 1;
            if let Some(u) = unit.as_ref() {
                acc.co_units[i]
                    .entry(path.to_string())
                    .or_default()
                    .insert(u.clone());
            }
        }
    }
}

/// 起点ごとに `CoChangeEntry` を構築し、全体ランキング順に並べて返す。
///
/// 集計は `stats` で完了しているので、ここでは閾値フィルタとランキングだけを行う。
/// 候補側の生成物ポリシーを解決する。
///
/// 判定対象を **`min_samples` を通る候補**に絞ってから解決する。生成物判定は
/// `.gitattributes` の問い合わせとファイル先頭の読み込みを伴うので、閾値で落ちる候補まで
/// 見ると I/O が候補総数に比例してしまう。`entries_for_source` 側でも生成物判定を
/// `min_samples` の直後に置くため、ここで解決した集合と判定対象は完全に一致する
/// (解決していないパスは `excluded` が常に None を返すので、取りこぼしても
/// 「除外しない」= 従来どおりの出力に倒れる)。
fn resolve_candidate_generated_policy(
    dir: &str,
    opts: &CoChangeOptions,
    stats: &ShardStats,
) -> GeneratedPolicy {
    if opts.include_generated {
        return GeneratedPolicy::disabled();
    }
    let candidates = stats
        .per_source_raw
        .iter()
        .flat_map(|per_source| per_source.iter())
        .filter(|(_, co)| **co >= opts.min_samples)
        .map(|(path, _)| path.as_str());
    GeneratedPolicy::resolve(dir, candidates)
}

/// 候補を落とすためのフィルタ一式。
///
/// どちらも「履歴上は共変更しているが、現在の変更候補として提示すべきでない」候補を
/// 落とす判定なので 1 つにまとめる (`Option<&HashSet>` と `&GeneratedPolicy` を位置引数で
/// 並べると呼び出し側で取り違えても型が合ってしまう組み合わせが増えるため)。
struct CandidateFilters<'a> {
    /// base リビジョンに存在するパス集合。`None` = 判定できなかったのでフィルタしない。
    base_tree_paths: Option<&'a HashSet<String>>,
    /// 生成物判定。`GeneratedPolicy::disabled()` なら常に「残す」。
    generated: &'a GeneratedPolicy,
}

fn assemble_entries(
    opts: &CoChangeOptions,
    index: &EvidenceIndex,
    stats: &ShardStats,
    min_denom: usize,
    filters: &CandidateFilters<'_>,
    diag: &mut CoChangeDiagnostics,
) -> Vec<CoChangeEntry> {
    let smoothing_on = !opts.disable_smoothing;
    let mut entries: Vec<CoChangeEntry> = Vec::new();
    for (i, source) in opts.source_files.iter().enumerate() {
        let evidence = &index.per_source[i];
        if stats.counted_denom[i] < min_denom {
            // 証拠が 1 件も無い起点は sources_without_evidence 側で数えているため、
            // ここでは「証拠はあったが分母が足りなかった」起点だけを数える。
            if !evidence.commits.is_empty() {
                diag.skipped_min_denominator += 1;
                diag.add_reason(CoChangeDiagnosticReason::BelowMinDenominator);
            }
            continue;
        }
        entries.extend(entries_for_source(
            i, source, evidence, stats, opts, filters, diag,
        ));
    }

    // 全体 ranking。smoothing 有効なら score 降順、無効なら confidence 降順。
    entries.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));
    entries
}

/// base リビジョンに存在するパス集合を 1 度だけ列挙する。
///
/// 共変更の候補は git 履歴から集めるため、**過去に削除済みのファイルも候補になる**。
/// 「現在は開けるファイルが無いパス」を「一緒に変更すべき候補」として提案しても
/// アクションが取れないので、候補生成後にここで落とす。
///
/// 候補ごとに `git cat-file` を叩くと候補数ぶんプロセスが起動するため、
/// `git ls-tree -r --name-only -z <rev>` で 1 回だけ棚卸しする。出力は cwd 相対なので
/// astro-sight の `--dir` 相対規約とそのまま一致する (`-z` で NUL 区切りにするため
/// 非 ASCII ファイル名の 8 進クォートも起きない)。
///
/// git の実行や revision 解決に失敗したら `None` を返し、**フィルタを適用しない**。
/// 判定できないことを理由に候補を消すと、本物の共変更を黙って落とすことになる
/// (この機能が消したいのは「削除済みと確定できた候補」だけ)。
fn collect_base_tree_paths(dir: &str, base: &str) -> Option<HashSet<String>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "-z", base])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|&b| b == 0)
            .filter(|seg| !seg.is_empty())
            .map(|seg| String::from_utf8_lossy(seg).into_owned())
            .collect(),
    )
}

/// 起点 1 件分の候補を閾値でふるい、ランキングして `per_source_limit` で切り詰める。
///
/// 閾値は raw `confidence` (と、明示指定時のみ `min_score`) に対して適用する。
/// 平滑化した `score` はランキング専用。score は分母が小さいほど 0 に引き寄せられる
/// shrinkage 推定値なので、閾値と突き合わせると「変更行 blame の分母が小さい起点は
/// 100% 共変更でも構造的に出力されない」(既定 β=8 では分母 2 で上限 0.27 < 0.3) 。
fn entries_for_source(
    i: usize,
    source: &str,
    evidence: &SourceEvidence,
    stats: &ShardStats,
    opts: &CoChangeOptions,
    filters: &CandidateFilters<'_>,
    diag: &mut CoChangeDiagnostics,
) -> Vec<CoChangeEntry> {
    let smoothing_on = !opts.disable_smoothing;
    let author_unit_active = opts.author_unit_window_days > 0;
    let score_gate_on = opts.min_score > 0.0;
    let counted_denom = stats.counted_denom[i];
    let local_denom_raw = counted_denom as f64;
    let evidence_set_len = evidence.commits.len();
    // blame 由来か履歴 fallback 由来かをエントリに引き継ぐ (既定 Blame は省略)。
    let evidence_kind = match evidence.mode {
        CoChangeEvidence::Blame => None,
        CoChangeEvidence::History => Some(CoChangeEvidence::History),
    };

    let mut out: Vec<CoChangeEntry> = Vec::new();
    for (cand, co) in &stats.per_source_raw[i] {
        let co = *co;
        diag.candidate_pairs += 1;
        // base リビジョンに存在しない候補 (過去のコミットで削除済み) は落とす。
        // 履歴上の共変更頻度は事実だが、現在の変更候補としては成立しない。
        if let Some(paths) = filters.base_tree_paths
            && !paths.contains(cand.as_str())
        {
            diag.filtered_deleted_candidates += 1;
            diag.add_reason(CoChangeDiagnosticReason::CandidateDeletedAtBase);
            continue;
        }
        if co < opts.min_samples {
            diag.filtered_min_samples += 1;
            diag.add_reason(CoChangeDiagnosticReason::BelowMinSamples);
            continue;
        }
        // 生成物は「一緒に直すべき相手」にならない。バッチが同時に書き出すという
        // 機械的な同時更新が、意味的な依存として高 confidence で並ぶのを止める。
        // `per_source_limit` の truncate より前なので、上位件数の枠も消費しない。
        if filters.generated.excluded(cand.as_str()).is_some() {
            diag.filtered_generated_candidates += 1;
            diag.add_reason(CoChangeDiagnosticReason::CandidateIsGenerated);
            continue;
        }
        let confidence = co as f64 / local_denom_raw;
        if confidence < opts.min_confidence {
            diag.filtered_min_confidence += 1;
            diag.add_reason(CoChangeDiagnosticReason::BelowMinConfidence);
            continue;
        }
        let score = if smoothing_on {
            stats.smoothed_score(i, cand, opts, author_unit_active)
        } else {
            confidence
        };
        if score_gate_on && score < opts.min_score {
            diag.filtered_min_score += 1;
            diag.add_reason(CoChangeDiagnosticReason::BelowMinScore);
            continue;
        }
        out.push(CoChangeEntry {
            file_a: source.to_string(),
            file_b: cand.clone(),
            co_changes: co,
            total_changes_a: evidence_set_len,
            total_changes_b: *stats.co_counts.get(cand).unwrap_or(&0),
            confidence,
            denominator: Some(counted_denom),
            score: Some(score),
            evidence: evidence_kind,
        });
    }
    out.sort_by(|a, b| compare_entries_by_ranking(a, b, smoothing_on));
    if opts.per_source_limit > 0 && out.len() > opts.per_source_limit {
        diag.truncated_per_source_limit += out.len() - opts.per_source_limit;
        out.truncate(opts.per_source_limit);
    }
    out
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
    /// 生成物と判定されたため証拠収集を行わなかった。
    ///
    /// 「証拠を作れなかった」(`sources_without_evidence`) とは別物なので、診断では
    /// 混ぜずに数える。混ぜると「解析が失敗した」と「意図的に除外した」を利用者が
    /// 区別できない。
    generated_excluded: bool,
}

impl SourceEvidence {
    fn empty(mode: CoChangeEvidence) -> Self {
        Self {
            commits: HashMap::new(),
            mode,
            no_changed_old_lines: false,
            new_file: false,
            git_failed: false,
            generated_excluded: false,
        }
    }

    /// 生成物として除外した起点 (証拠収集を行わなかった)。
    fn generated_excluded() -> Self {
        Self {
            generated_excluded: true,
            ..Self::empty(CoChangeEvidence::Blame)
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
///
/// `batched` は [`collect_changed_old_ranges_batch`] がまとめて取った変更行。無ければ
/// このファイルだけの `git diff` で取る。
fn collect_evidence_for_file(
    dir: &str,
    file: &str,
    base: &str,
    min_denom: usize,
    opts: &CoChangeOptions,
    batched: Option<&ChangedOldRanges>,
) -> SourceEvidence {
    let fetched;
    let changed = match batched {
        Some(changed) => changed,
        None => {
            fetched = collect_changed_old_ranges(dir, file, base, opts.changed_lines_from_index)
                .unwrap_or(ChangedOldRanges {
                    git_failed: true,
                    ..Default::default()
                });
            &fetched
        }
    };

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
#[derive(Debug, Default, PartialEq)]
struct ChangedOldRanges {
    /// blame 対象にする hunk 旧側 (start, count)。純粋追加 hunk は含まない。
    ranges: Vec<(u64, u64)>,
    /// base 側にファイルが無い (= base..HEAD で新規追加された)。
    new_file: bool,
    /// git コマンドが失敗した。
    git_failed: bool,
}

/// `git diff --unified=0 <base> -- <file>` から hunk 旧側 range を抽出する。
/// `from_index` なら `--cached` を付け、index との差分にする (review `--staged`)。
///
/// `new_file` は diff ヘッダの `--- /dev/null` で判定する
/// (base 側にファイルが無い = 新規追加 = 履歴 fallback も効かない)。
fn collect_changed_old_ranges(
    dir: &str,
    file: &str,
    base: &str,
    from_index: bool,
) -> Result<ChangedOldRanges> {
    // revision は `<base>` 単独 (`<base> HEAD` ではない)。2 revision 形式では
    // 未コミットの作業ツリー変更が hunk に現れず、pre-commit レビュー時に
    // 変更行 blame の証拠が常に空になる (他の --git コマンドと意味を揃える)。
    // 利用者の git 設定で出力形式が変わらないよう固定する。外部 diff が設定されていると
    // git ごと失敗して起点の証拠が空になり、色設定では `@@` 行を読めなくなる
    // (`GIT_DIFF_PARSEABLE_OUTPUT_ARGS` の doc 参照)。
    let mut command = Command::new("git");
    command
        .arg("diff")
        .args(crate::git_support::GIT_DIFF_PARSEABLE_OUTPUT_ARGS);
    if from_index {
        command.arg("--cached");
    }
    let diff_output = command
        .args(["--unified=0", base, "--", file])
        .env(LITERAL_PATHSPECS_ENV, "1")
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !diff_output.status.success() {
        return Ok(ChangedOldRanges {
            git_failed: true,
            ..Default::default()
        });
    }
    Ok(changed_old_ranges_from_diff(&String::from_utf8_lossy(
        &diff_output.stdout,
    )))
}

/// 1 ファイル分の `--unified=0` の出力から [`ChangedOldRanges`] を作る。ファイルごとの経路と
/// [`collect_changed_old_ranges_batch`] で共有する。
fn changed_old_ranges_from_diff(diff_text: &str) -> ChangedOldRanges {
    ChangedOldRanges {
        ranges: parse_hunk_old_ranges(diff_text),
        new_file: diff_text.lines().any(|l| l == "--- /dev/null"),
        git_failed: false,
    }
}

/// [`collect_changed_old_ranges_batch`] の 1 回の `git diff` に渡すパスの長さの合計の上限
/// (UTF-16 の符号単位)。Windows のコマンドラインは 32,767 単位までで、`git.exe` は本体の git を
/// 同じ引数で起動し直す。固定の引数と base、引数ごとの空白と引用符を足しても十分に収まる大きさにする。
const CHANGED_LINES_BATCH_MAX_PATH_UNITS: usize = 16 * 1024;

/// 起点ファイルの変更行を、ファイルごとに `git diff` を起動せずにまとめて取る。
///
/// `git diff <固定の書式> [--cached] --unified=0 --no-renames --relative <base> -- <f1> <f2> ...` の
/// 出力を `diff --git` の見出しでファイルごとのブロックに分け、[`collect_changed_old_ranges`] が
/// 1 ファイル分の出力に対して行うのと同じ解析をブロックに掛ける。
///
/// - 本体の diff (`--unified=3`) から導かないのは、git が `--unified=0` のときだけ末尾の共通部分を
///   切り詰めてから diff する (`xdiff-interface.c` の `trim_common_tail`) ため。変更のまとまり方が
///   変わって blame の範囲がずれることがある (このリポジトリの履歴 1,871 ファイル中 2 件)。
///   切り詰めはファイルごとに行われるので、一括の各ブロックはファイルごとの出力と一致する。
/// - `--no-renames`: ファイルごとの実行は pathspec が 1 ファイルにしか一致しないので rename / copy を
///   組めない。一括では利用者の `diff.renames` で組めてしまうため、明示的に切って同じにする。
/// - `--relative`: 見出しのパスを `dir` 相対 (= 渡したパス) にそろえ、対応付けに使う。中身は変わらない。
///
/// 一括で確定できたファイルだけを返し、返さなかったファイルは呼び出し側が 1 件ずつ取る。
/// 一括にしないもの: git が見出しで引用するパス (`"` / `\` / 制御文字。見出しから元のパスに
/// 戻せない)、ほかの起点を配下に持つパス (`-- p` は `p/a.rs` のブロックも返すので、ブロックを
/// パスの完全一致で振り分けると取りこぼす)。まとまりごとに、git の失敗・期待しない見出し
/// (パスがディレクトリに一致した等)・競合中の index の表示が出たら、そのまとまりを丸ごと返さない。
fn collect_changed_old_ranges_batch(
    dir: &str,
    files: &[&str],
    base: &str,
    from_index: bool,
) -> HashMap<String, ChangedOldRanges> {
    let requested: std::collections::BTreeSet<&str> = files.iter().copied().collect();
    let has_requested_descendant = |path: &str| {
        let parent = format!("{path}/");
        requested
            .range::<str, _>((
                std::ops::Bound::Included(parent.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|other| other.starts_with(&parent))
    };
    let mut result = HashMap::new();
    let mut chunk: Vec<&str> = Vec::new();
    let mut units = 0usize;
    for &file in &requested {
        if git_quotes_path(file) || has_requested_descendant(file) {
            continue;
        }
        let file_units = file.encode_utf16().count() + 3;
        if !chunk.is_empty() && units + file_units > CHANGED_LINES_BATCH_MAX_PATH_UNITS {
            result.extend(run_changed_lines_batch(dir, &chunk, base, from_index));
            chunk.clear();
            units = 0;
        }
        chunk.push(file);
        units += file_units;
    }
    if !chunk.is_empty() {
        result.extend(run_changed_lines_batch(dir, &chunk, base, from_index));
    }
    result
}

/// `core.quotepath=off` の git が diff の見出しで引用するパスか (`"` / `\` / 制御文字 / DEL)。
fn git_quotes_path(path: &str) -> bool {
    path.bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\')
}

/// [`collect_changed_old_ranges_batch`] の 1 まとまり分。確定できなければ空を返す。
fn run_changed_lines_batch(
    dir: &str,
    files: &[&str],
    base: &str,
    from_index: bool,
) -> HashMap<String, ChangedOldRanges> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotepath=off", "diff"])
        .args(crate::git_support::GIT_DIFF_PARSEABLE_OUTPUT_ARGS);
    if from_index {
        command.arg("--cached");
    }
    let Ok(output) = command
        .args(["--unified=0", "--no-renames", "--relative", base, "--"])
        .args(files)
        .env(LITERAL_PATHSPECS_ENV, "1")
        .current_dir(dir)
        .output()
    else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    let headers: HashMap<String, &str> = files
        .iter()
        .map(|&file| (format!("diff --git a/{file} b/{file}"), file))
        .collect();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut blocks: HashMap<&str, String> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in text.lines() {
        if line.starts_with("diff --") || line.starts_with("* Unmerged path ") {
            // 型の変更 (削除と追加の 2 ブロック) では同じパスの見出しが続けて出る。
            let Some(&file) = headers.get(line) else {
                return HashMap::new();
            };
            current = Some(file);
        }
        let Some(file) = current else {
            return HashMap::new();
        };
        let block = blocks.entry(file).or_default();
        block.push_str(line);
        block.push('\n');
    }
    files
        .iter()
        .map(|&file| {
            let diff_text = blocks.get(file).map_or("", String::as_str);
            (file.to_string(), changed_old_ranges_from_diff(diff_text))
        })
        .collect()
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
        .env(LITERAL_PATHSPECS_ENV, "1")
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
/// `min_denominator` を満たさない起点の代替証拠。`%H%x00%aE%x00%at` で SHA・author mail
/// (mailmap 適用済み)・author time を 1 起動でまとめて取り、blame 経路と同じ `BlameInfo` を組み立てる。
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
    // author mail は `.mailmap` を適用した `%aE` で取る。blame の `author-mail` は mailmap 適用済み
    // なので、`%ae` だと同じコミットが経路によって別の author になり、SHA ごとに最初に見た
    // 起点の値が採られる集計 (`sha_to_info`) で起点の並び順がスコアに漏れる。
    let mut args: Vec<String> = vec![
        "log".into(),
        "--format=%H%x00%aE%x00%at".into(),
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
        .env(LITERAL_PATHSPECS_ENV, "1")
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
///
/// **`--relative` は使わない。** `--relative` は `dir` 配下外を git 側で落とすため、
/// `total_files` (コミット規模) まで「ワークスペース内の変更ファイル数」に変わってしまい、
/// `max_files_per_commit` によるノイズ除外と `commit_size_weight` の意味が壊れる
/// (リポジトリ全体 500 ファイルの一括更新が、ワークスペース内 2 ファイルの集中コミットとして
/// 高スコアを作りうる)。リポジトリルート相対で全件受け取り、規模はその件数で評価したまま、
/// 共変更相手として返すパスだけ `workspace_prefix` を剥がして `dir` 相対に揃える
/// (起点 `source_files` は `is_contained_relative` で `dir` 相対に閉じているため、
/// 相手側だけルート相対だと同一エントリ内で基準が混ざる)。
/// `--dir` = リポジトリルートでは prefix が空で従来と完全に同一。
#[derive(Debug, Default, PartialEq)]
struct CommitFiles {
    /// コミット全体の変更ファイル数 (リポジトリルート基準、規模評価用)。
    total_files: usize,
    /// `dir` 配下の変更ファイル (`dir` 相対、共変更相手の候補)。
    workspace_files: Vec<String>,
}

/// git の失敗 (起動失敗 / 非 0 終了) は `Err` を返す。呼び出し側が「変更 0 件のコミット」と
/// 区別して件数を申告できるようにするため、空集合には畳まない (黙って畳むと
/// 「解析できなかったコミット」が「共変更が無かったコミット」に化ける)。
fn collect_files_in_commit(dir: &str, sha: &str, workspace_prefix: &str) -> Result<CommitFiles> {
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git diff-tree failed for {sha}: {}", stderr.trim()),
        ));
    }
    Ok(commit_files_from_lines(
        String::from_utf8_lossy(&output.stdout).lines(),
        workspace_prefix,
    ))
}

/// `git diff-tree --name-only` の出力の行から [`CommitFiles`] を作る。1 件ずつの経路
/// ([`collect_files_in_commit`]) と一括の経路 ([`run_diff_tree_batch`]) で共有する。
fn commit_files_from_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    workspace_prefix: &str,
) -> CommitFiles {
    let all: Vec<&str> = lines
        .into_iter()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let workspace_files = all
        .iter()
        .filter_map(|p| {
            if workspace_prefix.is_empty() {
                Some((*p).to_string())
            } else {
                p.strip_prefix(workspace_prefix).map(str::to_string)
            }
        })
        .filter(|p| !p.is_empty())
        .collect();
    CommitFiles {
        total_files: all.len(),
        workspace_files,
    }
}

/// `dir` のリポジトリルートからの相対 prefix (`"backend/"` 形式、ルートなら空) を返す。
///
/// 解決失敗は **fail-closed** (エラー伝播)。空文字にフォールバックすると、サブディレクトリ
/// 実行なのに全ルート相対パスが `workspace_files` に入り、配下外ファイルが候補に漏れる /
/// `source_set` の起点除外と `exclude_matcher` が別基準で評価される (起点自身が `app/a.rs`
/// として候補化しうる) ため、統一したはずのパス基準がここだけ破れる。
/// 「成功かつ空出力」= リポジトリルートのときだけ空 prefix として扱う。
fn workspace_prefix_for(dir: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-prefix"])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git rev-parse --show-prefix failed: {stderr}"),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
        // ロックファイルは glob ではなく正本テーブルで意味判定する。glob 側に列挙すると
        // ペア表との乖離で「Cargo.lock は除外されるが uv.lock は残る」という言語依存の
        // 非一貫性が生まれるため (ペアを 1 行足せば除外も追随する形にしておく)。
        if crate::models::dependency_files::is_dependency_lock_path(path) {
            return true;
        }
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

    /// commit-size weighting の集計が **加算順に依存しない**こと。
    ///
    /// 旧実装は `weighted_denom` / `per_source_weighted` に f64 を直接足し込んでいた。
    /// 加算元は `sha_to_sources` (HashMap) の反復順と rayon の reduce 順で決まるため
    /// 実行ごとに順序が変わり、非結合な f64 加算の結果が最終桁でぶれる。`score` は
    /// そのまま出力されるので「出力は決定論的に保つ」規約に反する。
    ///
    /// ヒストグラム (file_count → 度数、整数) で持てば加算は結合的になり、
    /// スコア再構成時は BTreeMap の file_count 昇順という固定順序で畳まれる。
    #[test]
    fn shard_stats_weighted_score_is_order_independent() {
        // 実際の commit_size_weight (= sqrt(pivot/file_count)) が無理数になるサイズを選ぶ。
        let file_counts: Vec<usize> = vec![11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53];
        let opts = CoChangeOptions {
            commit_size_pivot: 8,
            max_files_per_commit: 0, // 上限無効 (weight 0 での除外を起こさない)
            smoothing_alpha: 1.0,
            smoothing_beta: 8.0,
            ..CoChangeOptions::default()
        };

        // 対照: **旧実装の集計方式** (weight を f64 で直接足し込む) を再現すると、同じ入力でも
        // 集計順で score が変わる。これが再現しなければ本テストは空振りなので、
        // テスト内で明示的に確認する。
        let weights: Vec<f64> = file_counts
            .iter()
            .map(|&n| commit_size_weight(n, opts.commit_size_pivot, opts.max_files_per_commit))
            .collect();
        let legacy_score = |order: &[f64]| -> f64 {
            let denom = order.iter().fold(0.0, |a, w| a + w);
            let co = order.iter().fold(0.0, |a, w| a + w);
            (co + opts.smoothing_alpha) / (denom + opts.smoothing_alpha + opts.smoothing_beta)
        };
        let mut reversed_weights = weights.clone();
        reversed_weights.reverse();
        assert_ne!(
            legacy_score(&weights).to_bits(),
            legacy_score(&reversed_weights).to_bits(),
            "前提確認: 旧方式 (f64 直接加算) ではこの入力で score が順序依存になること"
        );

        // ヒストグラム集計を 2 通りの順で組み立てる。
        let build = |order: &[usize]| -> ShardStats {
            let mut stats = ShardStats::new(1);
            for &file_count in order {
                *stats.denom_weight_hist[0].entry(file_count).or_insert(0) += 1;
                *stats.per_source_weight_hist[0]
                    .entry("cand.rs".to_string())
                    .or_default()
                    .entry(file_count)
                    .or_insert(0) += 1;
            }
            stats
        };
        let mut reversed = file_counts.clone();
        reversed.reverse();

        let a = build(&file_counts).smoothed_score(0, "cand.rs", &opts, false);
        let b = build(&reversed).smoothed_score(0, "cand.rs", &opts, false);
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "集計順が変わっても score はビット単位で一致すること (a={a:?} b={b:?})"
        );

        // merge 経路も同じ不変条件を満たすこと (rayon の reduce 順に相当)。
        let merged_lr = ShardStats::merge(build(&file_counts[..6]), build(&file_counts[6..]))
            .smoothed_score(0, "cand.rs", &opts, false);
        let merged_rl = ShardStats::merge(build(&file_counts[6..]), build(&file_counts[..6]))
            .smoothed_score(0, "cand.rs", &opts, false);
        assert_eq!(
            merged_lr.to_bits(),
            merged_rl.to_bits(),
            "merge 順が変わっても score はビット単位で一致すること"
        );
        assert_eq!(
            merged_lr.to_bits(),
            a.to_bits(),
            "merge 経由でも単一 shard と同じ score になること"
        );
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

    /// 変更行 blame の起点となる hunk 旧側 range は、利用者の git 設定 (色・外部 diff) に
    /// 左右されない。色付きでは `@@` 行を読めず、外部 diff では unified diff 自体が出ないため、
    /// 起点の証拠が黙って空になっていた。
    #[test]
    fn collect_changed_old_ranges_ignores_user_diff_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(
            repo,
            &[("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n")],
            "init",
        );
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\n").unwrap();
        let dir_str = repo.to_str().unwrap();

        let expected = collect_changed_old_ranges(dir_str, "a.rs", "HEAD", false).unwrap();
        assert_eq!(
            expected.ranges,
            vec![(2, 1)],
            "前提: 既定設定では range を取れる"
        );
        for (key, value) in [
            ("color.ui", "always"),
            ("color.diff", "always"),
            ("diff.external", "echo"),
        ] {
            let git_config = |args: &[&str]| {
                assert!(
                    std::process::Command::new("git")
                        .args(args)
                        .current_dir(repo)
                        .status()
                        .unwrap()
                        .success()
                );
            };
            git_config(&["config", key, value]);
            let got = collect_changed_old_ranges(dir_str, "a.rs", "HEAD", false).unwrap();
            git_config(&["config", "--unset", key]);
            assert!(!got.git_failed, "{key}: git diff が失敗してはいけない");
            assert_eq!(got.ranges, expected.ranges, "{key}: hunk 旧側 range");
        }
    }

    /// `from_index` の変更行は index との差分から取る。同じファイルに未ステージの編集が
    /// あっても、blame の対象は index で変えた行のまま (対照: 作業ツリーとの差分は別の行)。
    #[test]
    fn collect_changed_old_ranges_from_index_ignores_unstaged_edits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        let lines = |edit: &dyn Fn(usize) -> Option<&'static str>| {
            (1..=10)
                .map(|n| {
                    format!(
                        "{}\n",
                        edit(n).map_or_else(|| format!("line{n}"), str::to_owned)
                    )
                })
                .collect::<String>()
        };
        commit_files(repo, &[("a.rs", &lines(&|_| None))], "init");
        std::fs::write(repo.join("a.rs"), lines(&|n| (n == 2).then_some("staged"))).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "a.rs"])
                .current_dir(repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            repo.join("a.rs"),
            lines(&|n| (n == 8).then_some("unstaged")),
        )
        .unwrap();
        let dir_str = repo.to_str().unwrap();

        let staged = collect_changed_old_ranges(dir_str, "a.rs", "HEAD", true).unwrap();
        assert!(!staged.git_failed);
        assert_eq!(staged.ranges, vec![(2, 1)]);
        let worktree = collect_changed_old_ranges(dir_str, "a.rs", "HEAD", false).unwrap();
        assert_eq!(worktree.ranges, vec![(8, 1)]);
    }

    fn git_ok(repo: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn numbered_lines(count: usize, edit: impl Fn(usize) -> Option<String>) -> String {
        (1..=count)
            .map(|n| format!("{}\n", edit(n).unwrap_or_else(|| format!("line{n}"))))
            .collect()
    }

    /// まとめて取った変更行が、起点ごとに `git diff --unified=0` を起動した結果と一致する。
    /// 変更・追加 (intent-to-add)・削除・rename の両側・バイナリ・末尾改行なし・空白や非 ASCII を
    /// 含むパス・未追跡・未変更・存在しないパスを、作業ツリーと index の両方、サブディレクトリからも
    /// 比べる。本体の diff (`--unified=3`) から導くと変更のまとまり方がずれることがあるので、
    /// 一括でも `--unified=0` のまま取っている。
    #[test]
    fn changed_lines_batch_matches_per_file_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        let renamed = numbered_lines(10, |n| Some(format!("fn r{n}() {{}}")));
        commit_files(
            repo,
            &[
                ("a.rs", &numbered_lines(30, |_| None)),
                ("deleted.rs", "one\ntwo\nthree\n"),
                ("renamed_old.rs", &renamed),
                ("no_eol.rs", "x\ny"),
                ("with space.rs", "a\nb\nc\n"),
                ("日本/c.rs", "a\nb\nc\n"),
                ("sub/s.rs", &numbered_lines(5, |_| None)),
                ("unchanged.rs", "same\n"),
            ],
            "init",
        );
        std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 0, 5]).unwrap();
        git_ok(repo, &["add", "bin.dat"]);
        git_ok(repo, &["commit", "-qm", "binary"]);

        std::fs::write(
            repo.join("a.rs"),
            numbered_lines(30, |n| match n {
                2 => Some("changed2".into()),
                10..=12 => Some(format!("changed{n}")),
                20 => Some("line20\ninserted".into()),
                25 => Some("changed25".into()),
                _ => None,
            }),
        )
        .unwrap();
        git_ok(repo, &["add", "a.rs"]);
        std::fs::write(
            repo.join("a.rs"),
            numbered_lines(30, |n| (n == 28).then(|| "unstaged28".into())),
        )
        .unwrap();
        std::fs::remove_file(repo.join("deleted.rs")).unwrap();
        git_ok(repo, &["mv", "renamed_old.rs", "renamed_new.rs"]);
        std::fs::write(
            repo.join("renamed_new.rs"),
            renamed.replace("fn r3()", "fn r3b()"),
        )
        .unwrap();
        std::fs::write(repo.join("bin.dat"), [0u8, 9, 9, 3, 0, 5]).unwrap();
        std::fs::write(repo.join("no_eol.rs"), "x\nz").unwrap();
        std::fs::write(repo.join("with space.rs"), "a\nB\nc\n").unwrap();
        std::fs::write(repo.join("日本/c.rs"), "a\nb\nC\n").unwrap();
        std::fs::write(
            repo.join("sub/s.rs"),
            numbered_lines(5, |n| (n == 4).then(|| "s4".into())),
        )
        .unwrap();
        std::fs::write(repo.join("added.rs"), "fn added() {}\n").unwrap();
        git_ok(repo, &["add", "-N", "added.rs"]);
        std::fs::write(repo.join("untracked.rs"), "fn untracked() {}\n").unwrap();

        let root_files = [
            "a.rs",
            "deleted.rs",
            "renamed_old.rs",
            "renamed_new.rs",
            "bin.dat",
            "no_eol.rs",
            "with space.rs",
            "日本/c.rs",
            "sub/s.rs",
            "added.rs",
            "untracked.rs",
            "unchanged.rs",
            "missing.rs",
        ];
        let root = repo.to_str().unwrap();
        let sub = repo.join("sub");
        let sub = sub.to_str().unwrap();
        for from_index in [false, true] {
            for (dir, files) in [(root, &root_files[..]), (sub, &["s.rs", "missing.rs"][..])] {
                let batch = collect_changed_old_ranges_batch(dir, files, "HEAD", from_index);
                for file in files {
                    let expected =
                        collect_changed_old_ranges(dir, file, "HEAD", from_index).unwrap();
                    assert_eq!(
                        batch.get(*file),
                        Some(&expected),
                        "{file} (from_index={from_index}, dir={dir})"
                    );
                }
            }
        }
        // 比較が空どうしにならないことの確認。
        let worktree = collect_changed_old_ranges_batch(root, &root_files, "HEAD", false);
        assert_eq!(worktree["a.rs"].ranges, vec![(28, 1)]);
        assert_eq!(worktree["deleted.rs"].ranges, vec![(1, 3)]);
        assert_eq!(worktree["renamed_old.rs"].ranges, vec![(1, 10)]);
        assert!(worktree["renamed_new.rs"].new_file);
        assert!(worktree["added.rs"].new_file);
        assert_eq!(worktree["untracked.rs"], ChangedOldRanges::default());
        let staged = collect_changed_old_ranges_batch(root, &root_files, "HEAD", true);
        assert_eq!(staged["a.rs"].ranges, vec![(2, 1), (10, 3), (25, 1)]);
    }

    /// 利用者の diff 設定 (色・外部 diff・接頭辞・rename 検出・相対パス) に左右されない。
    /// `diff.renames` が効くと一括の pathspec の中で rename を組み、削除と追加が 1 つのブロックに
    /// なってしまう (ファイルごとの実行では組めない)。
    #[test]
    fn changed_lines_batch_ignores_user_diff_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        let moved = numbered_lines(12, |n| Some(format!("fn m{n}() {{}}")));
        commit_files(
            repo,
            &[
                ("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n"),
                ("old.rs", &moved),
            ],
            "init",
        );
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\n").unwrap();
        std::fs::remove_file(repo.join("old.rs")).unwrap();
        std::fs::write(repo.join("new.rs"), &moved).unwrap();
        git_ok(repo, &["add", "-A"]);
        let dir_str = repo.to_str().unwrap();
        let files = ["a.rs", "old.rs", "new.rs"];
        let expected: HashMap<String, ChangedOldRanges> = files
            .iter()
            .map(|f| {
                let changed = collect_changed_old_ranges(dir_str, f, "HEAD", true).unwrap();
                (f.to_string(), changed)
            })
            .collect();
        assert_eq!(
            expected["old.rs"].ranges,
            vec![(1, 12)],
            "前提: 削除として取れる"
        );
        assert_eq!(
            collect_changed_old_ranges_batch(dir_str, &files, "HEAD", true),
            expected
        );
        for (key, value) in [
            ("color.ui", "always"),
            ("diff.external", "echo"),
            ("diff.noprefix", "true"),
            ("diff.mnemonicPrefix", "true"),
            ("diff.renames", "copies"),
            ("diff.relative", "true"),
        ] {
            git_ok(repo, &["config", key, value]);
            let got = collect_changed_old_ranges_batch(dir_str, &files, "HEAD", true);
            git_ok(repo, &["config", "--unset", key]);
            assert_eq!(got, expected, "{key}={value}");
        }
    }

    /// 一括の結果をパスの完全一致で振り分けられないものは、1 件ずつの経路に残す。
    /// - ほかの起点を配下に持つパス: `-- p` は `p/a.rs` のブロックも返す
    /// - 起点として渡していないファイルを配下に持つディレクトリ: そのまとまりごと
    /// - 競合中の index (`* Unmerged path`): そのまとまりごと
    #[test]
    fn changed_lines_batch_leaves_ambiguous_paths_to_per_file_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(
            repo,
            &[("p", "file\n"), ("q", "file\n"), ("a.rs", "a\n")],
            "init",
        );
        git_ok(repo, &["rm", "-q", "p", "q"]);
        commit_files(
            repo,
            &[("p/a.rs", "pa\n"), ("q/x.rs", "qx\n"), ("a.rs", "b\n")],
            "dirs",
        );
        let dir_str = repo.to_str().unwrap();
        let base = "HEAD~1";

        let nested =
            collect_changed_old_ranges_batch(dir_str, &["p", "p/a.rs", "a.rs"], base, false);
        assert!(!nested.contains_key("p"), "{nested:?}");
        for file in ["p/a.rs", "a.rs"] {
            assert_eq!(
                nested.get(file),
                Some(&collect_changed_old_ranges(dir_str, file, base, false).unwrap()),
                "{file}"
            );
        }
        let per_file_p = collect_changed_old_ranges(dir_str, "p", base, false).unwrap();
        assert_eq!(
            per_file_p.ranges,
            vec![(1, 1)],
            "`-- p` は削除された p の行を返す"
        );
        assert!(per_file_p.new_file, "`-- p` は p/a.rs の追加も返す");

        let directory = collect_changed_old_ranges_batch(dir_str, &["q", "a.rs"], base, false);
        assert!(directory.is_empty(), "{directory:?}");

        // 競合: 同じ行を変えた 2 つのブランチを merge する。
        git_ok(repo, &["checkout", "-q", "-b", "other"]);
        commit_files(repo, &[("a.rs", "other\n")], "other");
        git_ok(repo, &["checkout", "-q", "-"]);
        commit_files(repo, &[("a.rs", "main\n"), ("p/a.rs", "pb\n")], "main");
        let merge = Command::new("git")
            .args(["merge", "-q", "other"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(!merge.status.success(), "前提: a.rs が競合する");
        let conflicted =
            collect_changed_old_ranges_batch(dir_str, &["a.rs", "p/a.rs"], "HEAD~1", true);
        assert!(conflicted.is_empty(), "{conflicted:?}");
    }

    /// `"` を含むパスは git が見出しで引用するので、一括の対象にしない。
    #[cfg(unix)]
    #[test]
    fn changed_lines_batch_leaves_quoted_paths_to_per_file_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(repo, &[("q\"uote.rs", "a\n"), ("a.rs", "a\n")], "init");
        std::fs::write(repo.join("q\"uote.rs"), "b\n").unwrap();
        std::fs::write(repo.join("a.rs"), "b\n").unwrap();
        let dir_str = repo.to_str().unwrap();
        let batch =
            collect_changed_old_ranges_batch(dir_str, &["q\"uote.rs", "a.rs"], "HEAD", false);
        assert!(!batch.contains_key("q\"uote.rs"), "{batch:?}");
        assert_eq!(batch["a.rs"].ranges, vec![(1, 1)]);
    }

    /// 1 プロセスの `git diff-tree --stdin` で読んだ各コミットの変更ファイルが、コミットごとに
    /// `git diff-tree` を起動した結果と一致する。ルートコミット・空コミット・マージ・空白や非 ASCII を
    /// 含むパス・サブディレクトリの prefix を含め、存在しない SHA は 1 件ずつの経路に残す。
    #[test]
    fn diff_tree_batch_matches_per_commit_runs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        commit_files(
            repo,
            &[
                ("a.rs", "a\n"),
                ("sp ace/b.rs", "b\n"),
                ("日本/c.rs", "c\n"),
                ("sub/d.rs", "d\n"),
            ],
            "root",
        );
        commit_files(repo, &[("a.rs", "a2\n"), ("sub/d.rs", "d2\n")], "pair");
        git_ok(repo, &["commit", "-q", "--allow-empty", "-m", "empty"]);
        git_ok(repo, &["checkout", "-q", "-b", "side"]);
        commit_files(repo, &[("sub/side.rs", "s\n")], "side");
        git_ok(repo, &["checkout", "-q", "main"]);
        commit_files(repo, &[("日本/c.rs", "c2\n")], "main");
        git_ok(repo, &["merge", "-q", "--no-ff", "side", "-m", "merge"]);
        let listed = Command::new("git")
            .args(["rev-list", "--all"])
            .current_dir(repo)
            .output()
            .unwrap();
        let mut shas: Vec<String> = String::from_utf8(listed.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(shas.len(), 6);
        shas.insert(2, "1111111111111111111111111111111111111111".to_owned());
        let shas: Vec<&str> = shas.iter().map(String::as_str).collect();

        let sub = repo.join("sub");
        for (dir, prefix) in [(repo, ""), (sub.as_path(), "sub/")] {
            let dir = dir.to_str().unwrap();
            let mut batched: HashMap<usize, CommitFiles> = HashMap::new();
            let completed = run_diff_tree_batch(dir, &shas, &mut |i, lines| {
                batched.insert(
                    i,
                    commit_files_from_lines(lines.iter().map(String::as_str), prefix),
                );
            })
            .expect("diff-tree は存在しない SHA があっても正常に終わる");
            for (i, sha) in shas.iter().enumerate() {
                match collect_files_in_commit(dir, sha, prefix) {
                    Ok(expected) => {
                        assert!(completed[i], "{sha}");
                        assert_eq!(batched.get(&i), Some(&expected), "{sha} ({prefix})");
                    }
                    Err(_) => assert!(!completed[i] && !batched.contains_key(&i), "{sha}"),
                }
            }
        }
    }

    /// diff-tree が途中で終了した (壊れた tree を読んだ) ら、一括の結果は使わない。呼び出し側は
    /// そのまとまりを 1 件ずつ取り直し、失敗の件数は従来どおり数える
    /// (`analyze_cochange_reports_commit_scan_failures`)。
    #[test]
    fn diff_tree_batch_is_discarded_when_git_exits_abnormally() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("{i}\n")), ("sub/x.rs", &format!("{i}\n"))],
                &format!("c{i}"),
            );
        }
        let head = |rev: &str| {
            let out = Command::new("git")
                .args(["rev-parse", rev])
                .current_dir(repo)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let shas = [head("HEAD"), head("HEAD~1"), head("HEAD~2")];
        let shas: Vec<&str> = shas.iter().map(String::as_str).collect();
        let dir_str = repo.to_str().unwrap();
        assert!(run_diff_tree_batch(dir_str, &shas, &mut |_, _| {}).is_some());

        corrupt_subtree_object(repo, "HEAD~1");
        assert!(run_diff_tree_batch(dir_str, &shas, &mut |_, _| {}).is_none());
    }

    /// author 単位の集計は、同じコミットを blame 経由で見ても履歴 fallback 経由で見ても
    /// 同じ author にする (どちらも `.mailmap` を適用する)。以前は履歴 fallback だけが
    /// mailmap 前の `%ae` を使い、SHA ごとの author は最初に見た起点のものになるため、
    /// 起点の並び順でスコアが変わった (review は起点を HashSet の反復順で渡していた)。
    #[test]
    fn author_units_use_mailmap_identity_regardless_of_source_order() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        let commit = |files: &[(&str, &str)], author: (&str, &str), time: u64| {
            for (name, content) in files {
                std::fs::write(repo.join(name), content).unwrap();
            }
            let git = |args: &[&str]| {
                let output = Command::new("git")
                    .args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", author.0)
                    .env("GIT_AUTHOR_EMAIL", author.1)
                    .env("GIT_AUTHOR_DATE", format!("{time} +0000"))
                    .env("GIT_COMMITTER_DATE", format!("{time} +0000"))
                    .output()
                    .unwrap();
                assert!(output.status.success(), "git {args:?}");
            };
            git(&["add", "-A"]);
            git(&["commit", "-qm", "change"]);
        };
        let new = ("New Name", "new@example.com");
        let old = ("Old Name", "old@example.com");
        let start = 1_700_000_000;
        commit(
            &[
                (".mailmap", "New Name <new@example.com> <old@example.com>\n"),
                ("a.rs", "l1\nl2\nl3\nl4\nl5\n"),
                ("b.rs", "b1\nb2\n"),
                ("c.rs", "c0\n"),
            ],
            new,
            start,
        );
        // X は旧アドレスで書かれたコミット。a.rs (blame) と b.rs (履歴 fallback) の両方の証拠に入る。
        commit(
            &[
                ("a.rs", "l1\nx\nl3\nl4\nl5\n"),
                ("b.rs", "bx\nb2\n"),
                ("c.rs", "c1\n"),
            ],
            old,
            start + 100,
        );
        commit(
            &[("a.rs", "l1\nx\nz1\nl4\nl5\n"), ("c.rs", "c2\n")],
            new,
            start + 200,
        );
        commit(
            &[("a.rs", "l1\nx\nz1\nz2\nl5\n"), ("c.rs", "c3\n")],
            new,
            start + 300,
        );
        // a.rs は 2-4 行目 (blame で 3 コミット)、b.rs は 1 行目 (blame は 1 コミットなので履歴へ)。
        std::fs::write(repo.join("a.rs"), "l1\nA\nB\nC\nl5\n").unwrap();
        std::fs::write(repo.join("b.rs"), "B\nb2\n").unwrap();
        let score = |sources: [&str; 2]| {
            let opts = CoChangeOptions {
                source_files: sources.iter().map(|s| s.to_string()).collect(),
                ..CoChangeOptions::default()
            };
            let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
            result
                .entries
                .iter()
                .find(|e| e.file_a == "a.rs" && e.file_b == "c.rs")
                .and_then(|e| e.score)
                .expect("a.rs -> c.rs")
        };
        let forward = score(["a.rs", "b.rs"]);
        let backward = score(["b.rs", "a.rs"]);
        assert_eq!(forward, backward, "source order must not change the score");
        // X・Z1・Z2 は mailmap で同じ author の同じ週 = 1 unit: (1+α)/(1+α+β) = 2/10。
        assert!((forward - 0.2).abs() < 1e-12, "{forward}");
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

    /// `--dir` がサブディレクトリのとき、共変更相手のパスは `dir` 相対に揃うが、
    /// **コミット規模の判定はリポジトリ全体のファイル数**で行う。
    ///
    /// `git diff-tree --relative` で配下外を git 側に落とさせると、「リポジトリ全体では
    /// 巨大だがワークスペース内では数ファイル」の一括更新コミットが `max_files_per_commit`
    /// をすり抜け、集中コミットとして高い共変更スコアを作ってしまう。
    #[test]
    fn cochange_from_subdirectory_uses_repo_wide_commit_size() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // app/ 配下 (ワークスペース) と外側の両方を持つ構成。
        // 通常コミット 2 回: app/a.rs + app/b.rs だけを触る。
        for i in 0..2 {
            commit_files(
                repo,
                &[
                    (
                        "app/a.rs",
                        &format!("fn a() {{ {i} }}\nfn a2() {{ {i} }}\n"),
                    ),
                    ("app/b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        // 一括更新コミット: ワークスペース内は app/a.rs + app/noise.rs の 2 ファイルだけだが、
        // リポジトリ全体では 14 ファイル。--relative で配下外を落とすと 2 ファイルの
        // 集中コミットに見えてしまう。
        let mut bulk: Vec<(String, String)> = vec![
            (
                "app/a.rs".to_string(),
                "fn a() { 1 }\nfn a2() { 9 }\n".to_string(),
            ),
            ("app/noise.rs".to_string(), "// noise\n".to_string()),
        ];
        for i in 0..12 {
            bulk.push((format!("outside/bulk{i}.rs"), format!("// {i}\n")));
        }
        let bulk_refs: Vec<(&str, &str)> =
            bulk.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        commit_files(repo, &bulk_refs, "repo-wide bulk");
        commit_files(
            repo,
            &[("app/a.rs", "fn a() { x }\nfn a2() { x }\n")],
            "edit a",
        );

        let app_dir = repo.join("app");
        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            // ワークスペース内 2 ファイル < 5 < リポジトリ全体 14 ファイル。
            o.max_files_per_commit = 5;
        });
        let result = analyze_cochange(app_dir.to_str().unwrap(), &opts).unwrap();

        assert!(
            result.entries.iter().any(|e| e.file_b == "b.rs"),
            "共変更相手は --dir 相対で返るべき (app/b.rs ではなく b.rs): {:?}",
            result.entries
        );
        assert!(
            !result.entries.iter().any(|e| e.file_b == "noise.rs"),
            "リポジトリ全体で 14 ファイルの一括更新は規模超過として除外されるべき: {:?}",
            result.entries
        );
        assert!(
            !result
                .entries
                .iter()
                .any(|e| e.file_b.starts_with("outside/") || e.file_b.starts_with("app/")),
            "--dir 配下外は候補にせず、配下は prefix を剥がすべき: {:?}",
            result.entries
        );
        // 起点自身が候補に出ない = source_set (dir 相対) と候補パスの基準が一致している。
        // prefix を剥がし損ねると起点は `app/a.rs` になり source_set の `a.rs` と一致せず、
        // 自分自身を共変更相手として返してしまう。
        assert!(
            !result.entries.iter().any(|e| e.file_b == "a.rs"),
            "起点自身は共変更相手にならないべき (source_set と候補の基準一致): {:?}",
            result.entries
        );
    }

    /// テスト用のヘルパー。`<rev>` の `sub/` tree オブジェクトを壊し、`git diff-tree` だけを
    /// 失敗させる。blame は pathspec でルート直下のファイルしか辿らず `sub/` に降りないため、
    /// blame は成功したまま `diff-tree -r` (全体を再帰する) だけが落ちる。
    fn corrupt_subtree_object(repo: &std::path::Path, rev: &str) {
        let spec = format!("{rev}^{{tree}}:sub");
        let out = std::process::Command::new("git")
            .args(["rev-parse", &spec])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "rev-parse {spec} に失敗: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(oid.len(), 40, "SHA-1 リポジトリを前提にしている: {oid}");
        let path = repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        // loose object は読み取り専用 (444) で作られるため、上書きではなく置き換える。
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"corrupted").unwrap();
    }

    /// `git diff-tree` の失敗を「変更ファイル 0 件」に畳んで黙殺しない。
    ///
    /// 失敗コミットは共起を 1 件も出せないまま実効分母にだけ入るため、黙っていると
    /// 「解析できなかった」が「共変更が無かった」と区別できなくなる (診断の趣旨に反する)。
    /// 件数と理由を診断へ載せ、`commits_analyzed` (証拠集合 |C| のサイズ) は変えない。
    #[test]
    fn analyze_cochange_reports_commit_scan_failures() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                    ("sub/x.rs", &format!("fn x() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
        });

        // 対照ケース: 壊す前は失敗 0 件で、共変更もちゃんと出る。
        // これが無いと「別の理由で entries が空」になったときにテストが素通りする。
        let baseline = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();
        assert_eq!(
            baseline.diagnostics.commit_scan_failures, 0,
            "健全なリポでは失敗 0 件であるべき: {:?}",
            baseline.diagnostics
        );
        assert!(
            !baseline.entries.is_empty(),
            "対照ケースでは共変更が出るべき: {baseline:?}"
        );

        corrupt_subtree_object(repo, "HEAD~1");
        let result = analyze_cochange(repo.to_str().unwrap(), &opts).unwrap();

        assert!(
            result.diagnostics.sources_with_blame_evidence >= 1,
            "blame 側は成功したままでないと diff-tree 失敗の検証にならない: {:?}",
            result.diagnostics
        );
        assert!(
            result.diagnostics.commit_scan_failures > 0,
            "diff-tree の失敗が件数として申告されるべき: {:?}",
            result.diagnostics
        );
        assert!(
            result
                .diagnostics
                .reasons
                .contains(&CoChangeDiagnosticReason::CommitScanFailed),
            "0 件の理由として CommitScanFailed が載るべき: {:?}",
            result.diagnostics
        );
        assert_eq!(
            result.commits_analyzed, baseline.commits_analyzed,
            "commits_analyzed は証拠集合 |C| のサイズなので失敗分は差し引かない"
        );
    }

    /// 追加した診断フィールドは 0 のとき JSON に出さない (既存の消費側を壊さない)。
    #[test]
    fn commit_scan_failures_is_omitted_from_json_when_zero() {
        let mut diag = CoChangeDiagnostics::default();
        let json = serde_json::to_string(&diag).unwrap();
        assert!(
            !json.contains("commit_scan_failures"),
            "0 のときは出力から省略されるべき: {json}"
        );
        diag.commit_scan_failures = 2;
        let json = serde_json::to_string(&diag).unwrap();
        assert!(
            json.contains("\"commit_scan_failures\":2"),
            "非 0 のときは出力されるべき: {json}"
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
