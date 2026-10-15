use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

use crate::error::{AstroError, ErrorCode};
use crate::models::review::{ApiChanges, ApiSymbol, ApiSymbolChange, DeadSymbol, ReviewResult};
use crate::models::skip::SkipInfo;
use crate::service::AppService;

use super::api_changes::{api_diff_files, detect_api_changes};
use crate::output::{OutputOptions, serialize_cli_document};

use super::common::{MAX_INPUT_SIZE, log_phase, read_to_string_limited, timed, timed_ok};
use super::dead_code::{
    detect_dead_symbols, filter_dead_by_touched_symbols, filter_dead_by_wip_added,
    filter_diff_files_for_dead_code, resolve_dead_code_excludes,
    resolve_framework_globs_with_auto_detect,
};
use super::git_input::{DiffSourceResolution, GitDiffShortRead, resolve_diff_source, run_git_diff};
use hook::review_hook_output;
use missing_cochange::{ReviewCochangeInput, detect_missing_cochanges_scoped};

mod cochange_scope;
pub mod hook;
mod index_snapshot;
pub(crate) mod missing_cochange;
mod recent_cochange;
#[cfg(test)]
mod retry_tests;

// ---------------------------------------------------------------------------
// Review コマンド: impact / cochange / API surface diff / dead symbol 統合
// ---------------------------------------------------------------------------

/// `cmd_review` の引数一式。`CmdAstOpts` と同じく、隣接する同型引数
/// (`diff`/`diff_file`、`git`/`staged`/`hook`) の取り違えを型と名前で防ぐ。
pub struct CmdReviewOpts<'a> {
    pub dir: &'a str,
    pub diff: Option<&'a str>,
    pub diff_file: Option<&'a str>,
    pub git: bool,
    pub base: &'a str,
    pub staged: bool,
    pub min_confidence: f64,
    /// missing_cochanges が要求する最小共変更回数
    /// (0 = review 既定 `REVIEW_COCHANGE_MIN_SAMPLES` に倒す。engine 既定の 2 ではない)。
    pub cochange_min_samples: usize,
    pub output: OutputOptions,
    pub hook: bool,
    pub framework: Option<&'a str>,
    pub extra_exclude_dirs: &'a [String],
    pub extra_exclude_globs: &'a [String],
    pub dead_scope: crate::cli::DeadScope,
    pub strict_public_const_values: bool,
    pub strict_public_type_annotations: bool,
    pub include_wip_dead: bool,
    /// 生成物を cochange の起点・候補に残すか (グローバル `--include-generated` の解決結果)。
    pub include_generated: bool,
}

const REVIEW_MAX_ATTEMPTS: usize = 3;

pub fn cmd_review(service: &AppService, opts: &CmdReviewOpts<'_>) -> Result<()> {
    let git_input = opts.git && opts.diff.is_none() && opts.diff_file.is_none();
    let result = retry_review(git_input, |_| review_attempt(service, opts, |_, _| {}));
    if opts.hook && result.as_ref().is_err_and(review_input_changed) {
        eprintln!("review input kept changing; run review again");
    }
    result
}

fn review_input_changed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<AstroError>()
        .is_some_and(|error| error.code == ErrorCode::ReviewInputChanged)
}

/// Git 入力だけを差分取得からやり直す。結果や一時 snapshot は試行をまたいで持たない。
fn retry_review<T>(git_input: bool, mut attempt: impl FnMut(usize) -> Result<T>) -> Result<T> {
    let attempts = if git_input { REVIEW_MAX_ATTEMPTS } else { 1 };
    for number in 1..=attempts {
        match attempt(number) {
            Err(error)
                if git_input
                    && (review_input_changed(&error)
                        || error.downcast_ref::<GitDiffShortRead>().is_some()) => {}
            result => return result,
        }
    }
    Err(AstroError::new(
        ErrorCode::ReviewInputChanged,
        format!(
            "review input kept changing during analysis ({attempts} attempts); run review again"
        ),
    )
    .into())
}

/// 競合しうる読み込みと出力前の確認を一つの試行に閉じる。
/// before_check は同期テスト用。本番の空クロージャは最適化で除かれる。
fn review_attempt(
    service: &AppService,
    opts: &CmdReviewOpts<'_>,
    mut before_check: impl FnMut(&ReviewResult, &str),
) -> Result<()> {
    // 本体は従来の局所変数名のまま使うため、ここで一括分解する
    // (全フィールド Copy。本体側の書き換えゼロ = 引数取り違えの余地ゼロ)。
    let &CmdReviewOpts {
        dir,
        diff,
        diff_file,
        git,
        base,
        staged,
        min_confidence,
        cochange_min_samples,
        output,
        hook,
        framework,
        extra_exclude_dirs,
        extra_exclude_globs,
        dead_scope,
        strict_public_const_values,
        strict_public_type_annotations,
        include_wip_dead,
        include_generated,
    } = opts;
    // `--hook` の出力は Claude Code の Stop hook が消費する compact JSON 契約。
    // 明示的な `--format toon` は満たせないのでここで弾く (config 由来なら JSON に倒す)。
    if hook {
        output.ensure_json_protocol("review --hook")?;
    }
    // framework 指定は早期に検証して未知名はここで弾く (dead_symbols 検出に到達する前に)。
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    // 1. diff 取得（context コマンドと同じ入力方式）
    let (diff_input, truncations, pinned_base) =
        match resolve_diff_source(dir, diff, diff_file, git, base, staged)? {
            DiffSourceResolution::Diff {
                diff,
                truncations,
                base_commit,
            } => (diff, truncations, base_commit),
            // git 管理外: hook は完全 silent、通常は空結果 + skipped で exit 0。
            DiffSourceResolution::Skipped(skip) => {
                return emit_review_short_circuit(
                    hook,
                    output,
                    dir,
                    strict_public_const_values,
                    strict_public_type_annotations,
                    Some(skip),
                    Vec::new(),
                );
            }
            DiffSourceResolution::NotRequested => {
                let stdin = std::io::stdin();
                (
                    read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?,
                    Vec::new(),
                    None,
                )
            }
        };
    let analysis_base = pinned_base.as_deref().unwrap_or(base);
    // 構築前後の入力変化を検出するため、index と作業ツリーの両方を記録する。
    let working_tree_diff = if staged {
        pinned_base
            .as_deref()
            .map(|commit| run_git_diff(dir, commit, false))
            .transpose()?
    } else {
        None
    };
    if working_tree_diff.is_some() {
        ensure_review_diff_unchanged(
            dir,
            pinned_base.as_deref(),
            staged,
            &diff_input,
            working_tree_diff.as_deref(),
        )?;
    }

    if diff_input.trim().is_empty() {
        ensure_review_diff_unchanged(
            dir,
            pinned_base.as_deref(),
            staged,
            &diff_input,
            working_tree_diff.as_deref(),
        )?;
        return emit_review_short_circuit(
            hook,
            output,
            dir,
            strict_public_const_values,
            strict_public_type_annotations,
            None,
            truncations,
        );
    }

    // 2. impact 分析
    //
    // diff の全 changed file が case-insensitive 言語 (Xojo) のみで構成される場合は
    // review 全体を空結果として返す。
    //
    // v26.5 まで: tree-sitter-xojo の OOM 防止が主目的の重要な回避策。
    // v26.6 以降: tree-sitter-xojo を削除して OOM リスクは解消。だが lexer-only 言語の
    // cross-file refs と dead-code review は汎用名ノイズが多く実用精度が出ないため引き続き
    // skip する。本格的な lexer-only review 解析は将来の PR で対応予定。
    // `ASTRO_SIGHT_FORCE_CI_LANG_IMPACT=1` で従来挙動に戻せる (デバッグ用)。
    // diff は CI 言語判定 / changed_file_set / api_changes / dead_code filter / touched-symbols
    // で繰り返し参照するため、ここで一度だけ parse して再利用する。
    let diff_files = crate::engine::diff::parse_unified_diff(&diff_input);
    if crate::engine::impact::should_skip_ci_only_diff(&diff_files) {
        ensure_review_diff_unchanged(
            dir,
            pinned_base.as_deref(),
            staged,
            &diff_input,
            working_tree_diff.as_deref(),
        )?;
        log_phase("review.skip_ci_only", "applied", 0);
        return emit_review_short_circuit(
            hook,
            output,
            dir,
            strict_public_const_values,
            strict_public_type_annotations,
            None,
            truncations,
        );
    }

    let new_side = super::git_input::diff_new_side(diff, diff_file, git, staged);
    if pinned_base.is_none() {
        index_snapshot::verify_explicit_new_side(dir, &diff_input, &diff_files)?;
    }
    let StagedSource {
        snapshot,
        notice: snapshot_notice,
        keep_reference_downgrades_blocking,
    } = if matches!(new_side, crate::models::impact::DiffNewSide::Index) {
        timed_ok("staged_snapshot", || {
            prepare_staged_source(
                dir,
                analysis_base,
                &diff_input,
                index_snapshot::SnapshotBudget::DEFAULT,
            )
        })?
    } else {
        StagedSource::default()
    };
    let analysis_dir = match &snapshot {
        Some(snapshot) => snapshot.dir()?,
        None => dir,
    };
    let framework_globs = if snapshot.is_some() {
        resolve_framework_globs_with_auto_detect(framework, analysis_dir)?
    } else {
        framework_globs
    };

    let impact = timed_ok("context", || {
        // review の `--exclude-dir` / `--exclude-glob` は impact 解析と dead_symbols の
        // 両方に作用させる (v26.5.117 で挙動を統一)。
        // staged は changed file / refs / API / dead-code を同じ raw index tree から読む。
        let context_options = crate::models::impact::ContextAnalysisOptions {
            exclude_dirs: extra_exclude_dirs.to_vec(),
            exclude_globs: extra_exclude_globs.to_vec(),
            new_side: if snapshot.is_some() {
                crate::models::impact::DiffNewSide::WorkingTree
            } else {
                new_side
            },
        };
        service.analyze_context(&diff_input, analysis_dir, &context_options)
    })?;

    // 3. diff に含まれるファイルリストを収集
    let changed_file_set: HashSet<String> = diff_files
        .iter()
        .flat_map(|f| {
            let mut s = Vec::new();
            if f.new_path != "/dev/null" {
                s.push(f.new_path.clone());
            }
            if f.old_path != "/dev/null" {
                s.push(f.old_path.clone());
            }
            s
        })
        .collect();

    // 4. cochange 分析 → missing_cochanges 検出
    let cochange_report = timed_ok("cochange", || {
        detect_missing_cochanges_scoped(
            service,
            ReviewCochangeInput {
                dir,
                analysis_dir,
                changed_files: &changed_file_set,
                diff_files: &diff_files,
                min_confidence,
                min_samples: cochange_min_samples,
                base: Some(analysis_base),
                staged: matches!(new_side, crate::models::impact::DiffNewSide::Index),
                include_generated,
                exclude_dirs: extra_exclude_dirs,
                exclude_globs: extra_exclude_globs,
            },
        )
    })?;

    // 5. API 公開面の差分 (内容同一の rename も含めて見る。`api_diff_files` 参照)
    let mut api_changes = timed("api_changes", || {
        detect_api_changes(
            analysis_dir,
            analysis_base,
            &api_diff_files(&diff_files, &diff_input),
        )
    });
    if keep_reference_downgrades_blocking {
        keep_reference_based_downgrades_blocking(&mut api_changes);
    }

    // 6. dead symbol 検出
    let dead_opts = ReviewDeadSymbolsOpts {
        dir: analysis_dir,
        diff_input: &diff_input,
        diff_files: &diff_files,
        framework_globs: &framework_globs,
        extra_exclude_dirs,
        extra_exclude_globs,
        dead_scope,
        hook,
        include_wip_dead,
        api_added: &api_changes.added,
    };
    let dead_phase = timed_ok("dead_code", || review_dead_symbols(&dead_opts))?;
    let ReviewDeadSymbols {
        dead: dead_symbols,
        test_only: test_only_symbols,
        truncations: dead_truncations,
    } = dead_phase;
    let mut truncations = truncations;
    truncations.extend(snapshot_notice);
    truncations.extend(dead_truncations);
    truncations.append(&mut api_changes.parse_truncations);

    let result = ReviewResult {
        impact,
        missing_cochanges: cochange_report.missing,
        cochange_diagnostics: cochange_report.diagnostics,
        api_changes,
        dead_symbols,
        test_only_symbols,
        skipped: None,
        truncations,
    };

    before_check(&result, analysis_dir);
    ensure_review_diff_unchanged(
        dir,
        pinned_base.as_deref(),
        staged,
        &diff_input,
        working_tree_diff.as_deref(),
    )?;

    if hook {
        let blocking = review_hook_output(
            &result,
            analysis_dir,
            strict_public_const_values,
            strict_public_type_annotations,
            &diff_input,
            &diff_files,
        )?;
        // `process::exit` はデストラクタを走らせないため、staged の一時スナップショットを
        // exit の前に片付ける (Drop 任せにすると blocking のたびに一時ディレクトリが残る)。
        drop(snapshot);
        if blocking {
            std::process::exit(1);
        }
        return Ok(());
    }

    let text = serialize_cli_document(&result, output)?;
    info!(
        command = "review",
        dir = dir,
        output_bytes = text.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

/// 解析の間に index / 作業ツリー側が変わった場合、異なる時点の情報を混ぜた結果を返さない。
/// 基点には固定済みコミットを使い、HEAD の移動だけでは誤って失敗しない。
fn ensure_review_diff_unchanged(
    dir: &str,
    pinned_base: Option<&str>,
    staged: bool,
    original_diff: &str,
    original_working_tree_diff: Option<&str>,
) -> Result<()> {
    if let Some(base) = pinned_base {
        let current_diff = run_git_diff(dir, base, staged)?;
        if current_diff != original_diff {
            return Err(AstroError::new(
                ErrorCode::ReviewInputChanged,
                "review input changed during analysis; run review again",
            )
            .into());
        }
        if let Some(original) = original_working_tree_diff {
            let current = run_git_diff(dir, base, false)?;
            if current != original {
                return Err(AstroError::new(
                    ErrorCode::ReviewInputChanged,
                    "review working tree changed during analysis; run review again",
                )
                .into());
            }
        }
    }
    Ok(())
}

/// staged review の解析元。スナップショットを作れたらそれを、作れなければ縮退の申告を持つ。
#[derive(Default)]
struct StagedSource {
    snapshot: Option<index_snapshot::IndexSnapshot>,
    notice: Option<crate::models::truncation::TruncationInfo>,
    /// 作業ツリー由来の参照を根拠にした API の降格を blocking に戻すか。
    keep_reference_downgrades_blocking: bool,
}

/// staged review の index tree スナップショットを用意する。上限を超えたらエラーにせず、
/// スナップショット導入前の経路 (作業ツリー + 変更ファイルだけ index の blob) へ縮退する。
/// 作業ツリーの追跡ファイルが index と同じなら、作業ツリーから集めた参照は staged のものと
/// 一致する。違うときは参照の状況を根拠にした降格を確定させない (fail-closed)。
fn prepare_staged_source(
    dir: &str,
    base: &str,
    diff: &str,
    budget: index_snapshot::SnapshotBudget,
) -> Result<StagedSource> {
    Ok(
        match index_snapshot::IndexSnapshot::prepare(dir, base, diff, budget)? {
            index_snapshot::PreparedSnapshot::Ready(snapshot) => StagedSource {
                snapshot: Some(snapshot),
                ..StagedSource::default()
            },
            index_snapshot::PreparedSnapshot::OverBudget(over) => {
                let matches = working_tree_matches_index(dir);
                StagedSource {
                    snapshot: None,
                    notice: Some(over.truncation(matches)),
                    keep_reference_downgrades_blocking: !matches,
                }
            }
        },
    )
}

/// 作業ツリーの追跡ファイルが index と同じか (`git diff --quiet`)。hook が渡した
/// `GIT_INDEX_FILE` も staged diff と同じく尊重する。判定できなければ「違う」に倒す
/// (参照に依存する降格を確定させない側)。
fn working_tree_matches_index(dir: &str) -> bool {
    crate::git_support::git_command_for_dir(dir)
        .args(["diff", "--quiet", "--no-ext-diff", "--no-textconv"])
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// 作業ツリーから集めた参照を根拠にした API の降格を取り消し、blocking のバケットへ戻す。
///
/// staged review のスナップショットを作れなかったとき (上限超過) は、参照を作業ツリーから
/// 集める。作業ツリーに未ステージの変更があると、その参照は commit される内容と一致しない
/// ため、「呼び出し側が追随済み」「互換」「参照が残っていない」の証明にならない。
fn keep_reference_based_downgrades_blocking(api: &mut ApiChanges) {
    let closed = std::mem::take(&mut api.modified_closed_in_diff);
    api.modified.extend(closed);
    let compatible = std::mem::take(&mut api.compatible_modified);
    api.modified
        .extend(compatible.into_iter().map(|change| ApiSymbolChange {
            name: change.name,
            kind: change.kind,
            file: change.file,
            old_signature: change.old_signature,
            new_signature: change.new_signature,
            no_resolved_internal_callers: false,
            contract_change: None,
        }));
    let dead = std::mem::take(&mut api.removed_dead);
    api.removed.extend(dead);
    let unresolved = std::mem::take(&mut api.removed_unresolved);
    api.removed
        .extend(unresolved.into_iter().map(|removal| removal.symbol));
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn changed_worktree_or_index_invalidates_review_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(repo)
                    .status()
                    .expect("git")
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(repo.join("sample.rs"), "pub fn sample() -> i32 { 1 }\n").expect("write");
        git(&["add", "sample.rs"]);
        git(&["commit", "-qm", "initial"]);
        let dir = repo.to_str().expect("utf-8");

        let worktree_diff = run_git_diff(dir, "HEAD", false).expect("initial diff");
        fs::write(repo.join("sample.rs"), "pub fn sample() -> i32 { 2 }\n").expect("write");
        assert!(
            ensure_review_diff_unchanged(dir, Some("HEAD"), false, &worktree_diff, None).is_err()
        );

        git(&["add", "sample.rs"]);
        let staged_diff = run_git_diff(dir, "HEAD", true).expect("staged diff");
        fs::write(repo.join("sample.rs"), "pub fn sample() -> i32 { 3 }\n").expect("write");
        git(&["add", "sample.rs"]);
        assert!(ensure_review_diff_unchanged(dir, Some("HEAD"), true, &staged_diff, None).is_err());

        let staged_diff = run_git_diff(dir, "HEAD", true).expect("updated staged diff");
        let worktree_diff = run_git_diff(dir, "HEAD", false).expect("worktree snapshot");
        fs::write(repo.join("sample.rs"), "pub fn sample() -> i32 { 4 }\n").expect("write");
        assert!(
            ensure_review_diff_unchanged(
                dir,
                Some("HEAD"),
                true,
                &staged_diff,
                Some(&worktree_diff)
            )
            .is_err(),
            "staged が同じでも作業ツリー側の変化は破棄する"
        );
    }

    /// 上限を超えた staged tree はエラーにせず縮退し、申告を出す。作業ツリーの追跡ファイルが
    /// index と同じときだけ、作業ツリーの参照を根拠にした降格をそのまま使う。
    #[test]
    fn over_budget_staged_tree_falls_back_and_keeps_downgrades_only_when_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(repo)
                    .status()
                    .expect("git")
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(repo.join("a.py"), "def a():\n    return 1\n").expect("write");
        fs::write(repo.join("b.py"), "def b():\n    return 1\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);
        fs::write(repo.join("a.py"), "def a():\n    return 2\n").expect("write");
        git(&["add", "a.py"]);
        let dir = repo.to_str().expect("utf-8");
        // review は解決済みのコミット ID を渡す。
        let base = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf-8");
        let base = base.trim();
        let diff = run_git_diff(dir, base, true).expect("staged diff");
        let tiny = index_snapshot::SnapshotBudget {
            files: 1,
            bytes: u64::MAX,
        };

        let clean = prepare_staged_source(dir, base, &diff, tiny).expect("no hard error");
        assert!(clean.snapshot.is_none());
        assert!(!clean.keep_reference_downgrades_blocking);
        let notice = clean.notice.expect("fallback must be declared");
        assert_eq!(
            notice.reason,
            crate::models::truncation::TruncationReason::StagedSnapshotUnavailable
        );
        assert!(notice.message.contains("2 files"), "{}", notice.message);
        assert!(
            notice.message.contains("matched the index"),
            "{}",
            notice.message
        );

        // 未ステージの変更があると、作業ツリーの参照は commit される内容と一致しない。
        fs::write(repo.join("b.py"), "def b():\n    return 3\n").expect("write");
        let dirty = prepare_staged_source(dir, base, &diff, tiny).expect("no hard error");
        assert!(dirty.snapshot.is_none());
        assert!(dirty.keep_reference_downgrades_blocking);
        assert!(
            dirty
                .notice
                .expect("fallback must be declared")
                .message
                .contains("kept blocking")
        );

        // 対照: 上限内ならスナップショットを作り、申告も降格の取り消しもしない。
        let ready =
            prepare_staged_source(dir, base, &diff, index_snapshot::SnapshotBudget::DEFAULT)
                .expect("snapshot");
        assert!(ready.snapshot.is_some());
        assert!(ready.notice.is_none());
        assert!(!ready.keep_reference_downgrades_blocking);
    }

    /// 縮退時は、参照の状況を根拠にした降格 (mod_closed / mod_compat / rm_dead) を
    /// blocking のバケットへ戻す。それ以外のバケットは動かさない。
    #[test]
    fn reference_based_downgrades_return_to_blocking_buckets() {
        use crate::models::review::{
            CompatibleApiModification, UncertainApiRemoval, UnresolvedApiRemoval,
            UnresolvedRemovalConfidence, UnresolvedRemovalReason,
        };
        let symbol = |name: &str| ApiSymbol {
            name: name.into(),
            kind: "function".into(),
            file: "a.ts".into(),
            refs_internal: 0,
        };
        let change = |name: &str| ApiSymbolChange {
            name: name.into(),
            kind: "function".into(),
            file: "a.ts".into(),
            old_signature: Some(format!("{name}(a)")),
            new_signature: Some(format!("{name}(a, b)")),
            no_resolved_internal_callers: false,
            contract_change: None,
        };
        let mut api = ApiChanges {
            modified: vec![change("modified")],
            modified_closed_in_diff: vec![change("closed")],
            compatible_modified: vec![CompatibleApiModification {
                name: "compatible".into(),
                kind: "function".into(),
                file: "a.ts".into(),
                old_signature: Some("compatible(a, b?)".into()),
                new_signature: Some("compatible(a)".into()),
                reason: "trailing_optional_params_removed".into(),
            }],
            removed: vec![symbol("removed")],
            removed_dead: vec![symbol("dead")],
            removed_unresolved: vec![UnresolvedApiRemoval {
                symbol: symbol("unresolved"),
                reason: UnresolvedRemovalReason::UnresolvedReceiverTypeDemand,
                confidence: UnresolvedRemovalConfidence::Low,
                references: Vec::new(),
                field_owners: Vec::new(),
            }],
            uncertain_removals: vec![UncertainApiRemoval {
                name: "unverified".into(),
                kind: "function".into(),
                file: "lib.sh".into(),
                line: 0,
                old_signature: "unverified()".into(),
                reason: crate::models::truncation::TruncationReason::ParseErrorRegion,
            }],
            const_value_changes: vec![change("value")],
            ..ApiChanges::default()
        };
        keep_reference_based_downgrades_blocking(&mut api);
        let names =
            |items: &[ApiSymbolChange]| items.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&api.modified), ["modified", "closed", "compatible"]);
        assert_eq!(
            api.modified[2].new_signature.as_deref(),
            Some("compatible(a)")
        );
        assert!(api.modified_closed_in_diff.is_empty());
        assert!(api.compatible_modified.is_empty());
        assert_eq!(
            api.removed
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["removed", "dead", "unresolved"]
        );
        assert!(api.removed_dead.is_empty());
        assert!(api.removed_unresolved.is_empty());
        assert_eq!(api.uncertain_removals.len(), 1);
        assert_eq!(
            names(&api.const_value_changes),
            ["value"],
            "値だけの変更は動かさない"
        );
    }
}

/// 解析へ進まず空結果で打ち切る共通処理 (git 管理外 / 空 diff / CI 言語のみ の 3 経路)。
///
/// `--hook` は通常経路と同じ `review_hook_output` へ通す。`truncations` が空なら
/// `build_review_hook_json` が `None` を返すため従来どおり完全 silent、打ち切りが
/// あったときだけ `trunc` を申告する (informational なので blocking にはならない)。
/// 通常時は `skipped` / `truncations` 以外すべて既定値の `ReviewResult` を 1 行出力する。
///
/// **`--hook` で早期 return して truncations を捨ててはいけない**: 未追跡の巨大ファイルを
/// 除外した結果 diff が空になった場合に「全部見た」と読める沈黙になる。同じ入力で
/// `impact --hook` は `note:` 行で申告するので非対称でもあった。
fn emit_review_short_circuit(
    hook: bool,
    output: OutputOptions,
    dir: &str,
    strict_const_values: bool,
    strict_type_annotations: bool,
    skipped: Option<SkipInfo>,
    truncations: Vec<crate::models::truncation::TruncationInfo>,
) -> Result<()> {
    let result = ReviewResult {
        skipped,
        truncations,
        ..Default::default()
    };

    if hook {
        // 解析へ進まない経路なので impact も空 = diff 内の解決判定も要らない。
        if review_hook_output(
            &result,
            dir,
            strict_const_values,
            strict_type_annotations,
            "",
            &[],
        )? {
            std::process::exit(1);
        }
        return Ok(());
    }

    let text = serialize_cli_document(&result, output)?;
    print!("{text}");
    Ok(())
}

/// dead symbol 検出フェーズの入力。引数が多いため `CmdAstOpts` と同じく struct にまとめる。
struct ReviewDeadSymbolsOpts<'a> {
    dir: &'a str,
    diff_input: &'a str,
    diff_files: &'a [crate::models::impact::DiffFile],
    framework_globs: &'a [String],
    extra_exclude_dirs: &'a [String],
    extra_exclude_globs: &'a [String],
    dead_scope: crate::cli::DeadScope,
    hook: bool,
    include_wip_dead: bool,
    /// 同一 diff で新規 export されたシンボル (WIP dead 抑止の判定材料)。
    api_added: &'a [ApiSymbol],
}

/// dead symbol 検出 (framework プリセット + ユーザ指定 exclude を適用)。
/// review では vendor / tests / build を常に除外する固定挙動。
/// 必要になった段階で dead-code と同様の --include-* オプションを追加する。
///
/// `dir` を canonicalize できない場合は空結果 (エラーにしない)。
/// `review` の dead-symbol フェーズの結果。
struct ReviewDeadSymbols {
    dead: Vec<DeadSymbol>,
    test_only: Vec<DeadSymbol>,
    /// 参照カウントの走査対象から外れた「解析できないソース」の申告。
    /// dead が 1 件も無ければ誤検出のしようが無いので空 (走査自体を行わない)。
    truncations: Vec<crate::models::truncation::TruncationInfo>,
}

fn review_dead_symbols(opts: &ReviewDeadSymbolsOpts<'_>) -> Result<ReviewDeadSymbols> {
    let Ok(canonical_dir) = std::fs::canonicalize(opts.dir) else {
        return Ok(ReviewDeadSymbols {
            dead: Vec::new(),
            test_only: Vec::new(),
            truncations: Vec::new(),
        });
    };

    let default_excludes = resolve_dead_code_excludes(false, false, false);
    let mut excludes: Vec<&str> = default_excludes.to_vec();
    for name in opts.extra_exclude_dirs {
        excludes.push(name.as_str());
    }
    let mut combined_globs: Vec<&str> = opts.framework_globs.iter().map(String::as_str).collect();
    for pat in opts.extra_exclude_globs {
        combined_globs.push(pat.as_str());
    }
    let files = filter_diff_files_for_dead_code(
        &canonical_dir,
        opts.diff_files,
        &excludes,
        &combined_globs,
        None,
    )?;
    let detection = detect_dead_symbols(opts.dir, &files, false);
    let (dead_symbols, test_only_symbols) = (detection.dead, detection.test_only);
    // dead-scope=touched-symbols: 宣言行が diff の `+` 行と重ならない dead を除外。
    // `--hook` のデフォルトで「changed file 内の元から存在した dead」の
    // ノイズを抑える (Issue: zod-inferred-types-pre-existing-dead)。
    let dead_symbols = if matches!(opts.dead_scope, crate::cli::DeadScope::TouchedSymbols) {
        filter_dead_by_touched_symbols(opts.dir, dead_symbols, opts.diff_input, opts.diff_files)
    } else {
        dead_symbols
    };
    // WIP dead 抑止: 同一 diff で新規 export された (= api_changes.added に挙がる)
    // シンボルは「多段実装中に consumer 結線が後続コミット予定」の純粋ヘルパー追加
    // 等に該当しうるため、`review --hook` のデフォルトで dead 警告から外す。
    // `--include-wip-dead` で旧挙動 (全 dead を返す) に戻せる。`--hook` 無しの通常
    // `review` JSON では従来通り全 dead を残す ― レビュアーが api.added と dead の
    // 両者を見て総合判断する想定で、自動 hook ノイズ抑止のスコープを外している
    // (Issue 2026-06-25-wip-dead-symbol-during-incremental-impl)。
    let dead_symbols = if opts.hook && !opts.include_wip_dead {
        filter_dead_by_wip_added(dead_symbols, opts.api_added)
    } else {
        dead_symbols
    };

    // 解析できないソース (`.vue` / `.svelte` 等) があると、その中の参照を数えられないまま
    // dead と断定してしまう (生きているシンボルを dead と報告する最悪方向の誤り)。
    // ただし dead が 1 件も無ければ誤検出のしようが無いので、その場合は
    // 追加のディレクトリ走査を行わない (`--hook` のホットパスにコストを足さない)。
    let mut truncations = if dead_symbols.is_empty() && test_only_symbols.is_empty() {
        Vec::new()
    } else {
        crate::engine::refs::collect_files_scan_with_excludes(
            &canonical_dir,
            None,
            &excludes,
            &combined_globs,
            crate::engine::refs::FileScanOptions::default(),
        )
        .map(|collection| collection.unanalyzable_truncations(&canonical_dir))
        .unwrap_or_default()
    };
    // 参照を数えられなかったファイル (読み込み・構文解析の失敗) は、dead を保留した結果
    // dead が 0 件でも申告する (「全部数えた」と読めないように)。
    truncations.extend(detection.uncounted);

    Ok(ReviewDeadSymbols {
        dead: dead_symbols,
        test_only: test_only_symbols,
        truncations,
    })
}

#[cfg(test)]
mod review_command_tests {
    use super::*;

    /// `--hook` の短絡は 3 経路 (git 管理外 / 空 diff / CI 言語のみ) すべてで無出力 Ok。
    #[test]
    fn short_circuit_is_silent_under_hook() {
        emit_review_short_circuit(
            true,
            OutputOptions::default(),
            ".",
            false,
            false,
            None,
            Vec::new(),
        )
        .expect("hook short-circuit must succeed");
        emit_review_short_circuit(
            true,
            OutputOptions::default(),
            ".",
            false,
            false,
            Some(SkipInfo::not_git_repository()),
            Vec::new(),
        )
        .expect("hook short-circuit must succeed");
    }

    /// dir を canonicalize できない場合、dead 検出はエラーではなく空結果で返す。
    #[test]
    fn dead_symbols_are_empty_when_dir_cannot_be_canonicalized() {
        let opts = ReviewDeadSymbolsOpts {
            dir: "/nonexistent/astro-sight/review/dir",
            diff_input: "",
            diff_files: &[],
            framework_globs: &[],
            extra_exclude_dirs: &[],
            extra_exclude_globs: &[],
            dead_scope: crate::cli::DeadScope::All,
            hook: false,
            include_wip_dead: false,
            api_added: &[],
        };

        let phase = review_dead_symbols(&opts).expect("missing dir must not be an error");
        assert!(phase.dead.is_empty());
        assert!(phase.test_only.is_empty());
        // dir が存在しない = 走査もできないので申告も空
        assert!(phase.truncations.is_empty());
    }
}
