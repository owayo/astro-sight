use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

use crate::models::review::{ApiSymbol, DeadSymbol, ReviewResult};
use crate::models::skip::SkipInfo;
use crate::service::AppService;

use super::api_changes::{detect_api_changes, detect_missing_cochanges};
use super::common::{
    MAX_INPUT_SIZE, log_phase, read_to_string_limited, serialize_output, timed, timed_ok,
};
use super::dead_code::{
    detect_dead_symbols_from_files, filter_dead_by_touched_symbols, filter_dead_by_wip_added,
    filter_diff_files_for_dead_code, resolve_dead_code_excludes,
    resolve_framework_globs_with_auto_detect,
};
use super::git_input::{DiffSourceResolution, resolve_diff_source};
use hook::review_hook_output;

pub mod hook;

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
    pub pretty: bool,
    pub hook: bool,
    pub framework: Option<&'a str>,
    pub extra_exclude_dirs: &'a [String],
    pub extra_exclude_globs: &'a [String],
    pub dead_scope: crate::cli::DeadScope,
    pub strict_public_const_values: bool,
    pub include_wip_dead: bool,
}

pub fn cmd_review(service: &AppService, opts: &CmdReviewOpts<'_>) -> Result<()> {
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
        pretty,
        hook,
        framework,
        extra_exclude_dirs,
        extra_exclude_globs,
        dead_scope,
        strict_public_const_values,
        include_wip_dead,
    } = opts;
    // framework 指定は早期に検証して未知名はここで弾く (dead_symbols 検出に到達する前に)。
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    // 1. diff 取得（context コマンドと同じ入力方式）
    let diff_input = match resolve_diff_source(dir, diff, diff_file, git, base, staged)? {
        DiffSourceResolution::Diff(s) => s,
        // git 管理外: hook は完全 silent、通常は空結果 + skipped で exit 0。
        DiffSourceResolution::Skipped(skip) => {
            return emit_review_short_circuit(hook, pretty, Some(skip));
        }
        DiffSourceResolution::NotRequested => {
            let stdin = std::io::stdin();
            read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
        }
    };

    if diff_input.trim().is_empty() {
        return emit_review_short_circuit(hook, pretty, None);
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
        log_phase("review.skip_ci_only", "applied", 0);
        return emit_review_short_circuit(hook, pretty, None);
    }

    let impact = timed_ok("context", || {
        // review の `--exclude-dir` / `--exclude-glob` は impact 解析と dead_symbols の
        // 両方に作用させる (v26.5.117 で挙動を統一)。
        let context_options = crate::models::impact::ContextAnalysisOptions {
            exclude_dirs: extra_exclude_dirs.to_vec(),
            exclude_globs: extra_exclude_globs.to_vec(),
        };
        service.analyze_context(&diff_input, dir, &context_options)
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
    let missing_cochanges = timed_ok("cochange", || {
        detect_missing_cochanges(service, dir, &changed_file_set, min_confidence, Some(base))
    })?;

    // 5. API 公開面の差分
    let api_changes = timed("api_changes", || detect_api_changes(dir, base, &diff_files));

    // 6. dead symbol 検出
    let dead_opts = ReviewDeadSymbolsOpts {
        dir,
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
    let (dead_symbols, test_only_symbols) =
        timed_ok("dead_code", || review_dead_symbols(&dead_opts))?;

    let result = ReviewResult {
        impact,
        missing_cochanges,
        api_changes,
        dead_symbols,
        test_only_symbols,
        skipped: None,
    };

    if hook {
        return review_hook_output(&result, dir, strict_public_const_values);
    }

    let output = serialize_output(&result, pretty)?;
    info!(
        command = "review",
        dir = dir,
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

/// 解析へ進まず空結果で打ち切る共通処理 (git 管理外 / 空 diff / CI 言語のみ の 3 経路)。
///
/// `--hook` は完全 silent (無出力)、通常時は `skipped` 以外すべて既定値の `ReviewResult`
/// を 1 行出力して exit 0 とする。
fn emit_review_short_circuit(hook: bool, pretty: bool, skipped: Option<SkipInfo>) -> Result<()> {
    if hook {
        return Ok(());
    }

    let result = ReviewResult {
        skipped,
        ..Default::default()
    };
    let output = serialize_output(&result, pretty)?;
    println!("{output}");
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
fn review_dead_symbols(
    opts: &ReviewDeadSymbolsOpts<'_>,
) -> Result<(Vec<DeadSymbol>, Vec<DeadSymbol>)> {
    let Ok(canonical_dir) = std::fs::canonicalize(opts.dir) else {
        return Ok((Vec::new(), Vec::new()));
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
    let (dead_symbols, test_only_symbols) = detect_dead_symbols_from_files(opts.dir, &files);
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

    Ok((dead_symbols, test_only_symbols))
}

#[cfg(test)]
mod review_command_tests {
    use super::*;

    /// `--hook` の短絡は 3 経路 (git 管理外 / 空 diff / CI 言語のみ) すべてで無出力 Ok。
    #[test]
    fn short_circuit_is_silent_under_hook() {
        emit_review_short_circuit(true, false, None).expect("hook short-circuit must succeed");
        emit_review_short_circuit(true, true, Some(SkipInfo::not_git_repository()))
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

        let (dead, test_only) =
            review_dead_symbols(&opts).expect("missing dir must not be an error");
        assert!(dead.is_empty());
        assert!(test_only.is_empty());
    }
}
