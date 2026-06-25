use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::models::cochange::{CoChangeOptions, CoChangeResult};
use crate::models::review::ReviewResult;
use crate::models::skip::SkipInfo;
use crate::service::{AppService, AstParams};

mod common;

#[cfg(test)]
pub(crate) use common::read_bytes_limited_and_drain;
pub use common::{MAX_INPUT_SIZE, classify_error, read_paths_file_limited, serialize_output};
pub(crate) use common::{
    cache_hash_for_path, log_phase, read_file_to_string_limited, read_to_string_limited,
};

// ---------------------------------------------------------------------------
// 単一ファイル系コマンド（キャッシュ・pretty 対応）
// ---------------------------------------------------------------------------

pub struct CmdAstOpts<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub col: Option<usize>,
    pub end_line: Option<usize>,
    pub end_col: Option<usize>,
    pub depth: usize,
    pub context_lines: usize,
    pub full: bool,
    pub no_cache: bool,
    pub pretty: bool,
}

pub fn cmd_ast(service: &AppService, opts: &CmdAstOpts<'_>) -> Result<()> {
    let utf8_path = camino::Utf8Path::new(opts.path);
    let source = parser::read_file(utf8_path)?;
    let hash = cache_hash_for_path(utf8_path, &source);
    let use_cache = !opts.no_cache && !opts.pretty;

    fn opt_key(v: Option<usize>) -> String {
        match v {
            Some(n) => n.to_string(),
            None => "N".to_string(),
        }
    }
    let mode = if opts.full { "full" } else { "compact" };
    let cache_key = format!(
        "v2_ast_{}_{}_{}_{}_{}_{}_{}",
        opt_key(opts.line),
        opt_key(opts.col),
        opt_key(opts.end_line),
        opt_key(opts.end_col),
        opts.depth,
        opts.context_lines,
        mode
    );

    if use_cache
        && let Ok(cache) = CacheStore::new()
        && let Some(cached) = cache.get(&hash, &cache_key)
    {
        info!(
            command = "ast",
            path = opts.path,
            output_bytes = cached.len(),
            cached = true,
            "💾 cache hit"
        );
        std::io::Write::write_all(&mut std::io::stdout(), &cached)?;
        return Ok(());
    }

    let params = AstParams {
        path: opts.path,
        line: opts.line,
        col: opts.col,
        end_line: opts.end_line,
        end_col: opts.end_col,
        depth: opts.depth,
        context_lines: opts.context_lines,
    };
    let response = service.extract_ast(&params)?;

    let mut output = if opts.full {
        serialize_output(&response, opts.pretty)?
    } else {
        serialize_output(&response.to_compact_ast(), opts.pretty)?
    };
    output.push('\n');

    info!(
        command = "ast",
        path = opts.path,
        output_bytes = output.len(),
        "command completed"
    );

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, &cache_key, output.as_bytes());
    }

    print!("{output}");
    Ok(())
}

pub fn cmd_symbols_dir(
    service: &AppService,
    dir: &str,
    glob: Option<&str>,
    doc: bool,
    full: bool,
) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    let files = crate::engine::refs::collect_files(&canonical_dir, glob)?;
    let file_paths: Vec<String> = files
        .iter()
        .filter_map(|p| p.to_str().map(|s| s.to_string()))
        .collect();
    batch_symbols(service, &file_paths, doc, full, Some(&canonical_dir))
}

pub fn cmd_symbols(
    service: &AppService,
    path: &str,
    no_cache: bool,
    pretty: bool,
    doc: bool,
    full: bool,
) -> Result<()> {
    let utf8_path = camino::Utf8Path::new(path);
    let source = parser::read_file(utf8_path)?;
    let hash = cache_hash_for_path(utf8_path, &source);
    let use_cache = !no_cache && !pretty;

    // v3_: Symbol に enclosing container フィールド追加 (compact では `cn` キー)
    let cache_key = if full {
        "v3_symbols_full"
    } else if doc {
        "v3_symbols_doc"
    } else {
        "v3_symbols"
    };

    if use_cache
        && let Ok(cache) = CacheStore::new()
        && let Some(cached) = cache.get(&hash, cache_key)
    {
        info!(
            command = "symbols",
            path = path,
            output_bytes = cached.len(),
            cached = true,
            "💾 cache hit"
        );
        std::io::Write::write_all(&mut std::io::stdout(), &cached)?;
        return Ok(());
    }

    let response = service.extract_symbols(path)?;

    let mut output = if full {
        serialize_output(&response, pretty)?
    } else {
        let compact = response.to_compact_symbols(doc);
        serialize_output(&compact, pretty)?
    };
    output.push('\n');

    info!(
        command = "symbols",
        path = path,
        output_bytes = output.len(),
        "command completed"
    );

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, cache_key, output.as_bytes());
    }

    print!("{output}");
    Ok(())
}

pub fn cmd_calls(
    service: &AppService,
    path: &str,
    function: Option<&str>,
    pretty: bool,
) -> Result<()> {
    let result = service.extract_calls(path, function)?;
    let output = if pretty {
        serialize_output(&result, true)?
    } else {
        serialize_output(&result.to_compact(), false)?
    };
    info!(command = "calls", path = path, function = ?function, output_bytes = output.len(), "command completed");
    println!("{output}");
    Ok(())
}

pub fn cmd_imports(service: &AppService, path: &str, pretty: bool) -> Result<()> {
    let result = service.extract_imports(path)?;
    let output = serialize_output(&result, pretty)?;
    info!(
        command = "imports",
        path = path,
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

pub fn cmd_lint(
    service: &AppService,
    path: &str,
    rules: &[crate::models::lint::Rule],
    pretty: bool,
) -> Result<()> {
    let result = service.lint_file(path, rules)?;
    let output = serialize_output(&result, pretty)?;
    info!(
        command = "lint",
        path = path,
        rules_count = rules.len(),
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

pub fn cmd_sequence(
    service: &AppService,
    path: &str,
    function: Option<&str>,
    pretty: bool,
) -> Result<()> {
    let result = service.generate_sequence(path, function)?;
    let output = serialize_output(&result, pretty)?;
    info!(command = "sequence", path = path, function = ?function, output_bytes = output.len(), "command completed");
    println!("{output}");
    Ok(())
}

pub fn cmd_refs(
    service: &AppService,
    name: &str,
    dir: &str,
    glob: Option<&str>,
    pretty: bool,
) -> Result<()> {
    let result = service.find_references(name, dir, glob)?;
    let output = serialize_output(&result, pretty)?;
    info!(command = "refs", name = name, dir = dir, glob = ?glob, output_bytes = output.len(), "command completed");
    println!("{output}");
    Ok(())
}

pub fn cmd_refs_batch(
    service: &AppService,
    names: &[String],
    dir: &str,
    glob: Option<&str>,
) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut total_refs = 0usize;

    // find_references_batch が内部で名前を chunk 分割しつつディレクトリ走査を 1 回に
    // 集約するため、ここでは全名を 1 回で渡す（以前は呼び出し側で chunk 分割していたが
    // chunk 毎に walk し直していた）。service は入力順を保った `Vec<RefsResult>` を返すので
    // NDJSON 出力も names 順を維持する。
    let results = service.find_references_batch(names, dir, glob)?;
    for result in &results {
        total_refs += result.references.len();
        let line = serde_json::to_string(result)?;
        writeln!(out, "{line}")?;
    }

    info!(
        command = "refs_batch",
        names_count = names.len(),
        total_refs = total_refs,
        "command completed"
    );
    Ok(())
}

pub fn cmd_cochange(
    service: &AppService,
    dir: &str,
    opts: &CoChangeOptions,
    pretty: bool,
    skipped: Option<SkipInfo>,
) -> Result<()> {
    if let Some(skip) = skipped {
        // git 管理外 (起点ファイル無し): 空の entries + skipped を返して exit 0。
        let result = CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
            skipped: Some(skip),
        };
        let output = serialize_output(&result, pretty)?;
        println!("{output}");
        return Ok(());
    }
    let result = service.analyze_cochange(dir, opts)?;
    let output = serialize_output(&result, pretty)?;
    info!(
        command = "cochange",
        dir = dir,
        source_files = opts.source_files.len(),
        base = ?opts.base,
        min_confidence = opts.min_confidence,
        min_samples = opts.min_samples,
        max_files_per_commit = opts.max_files_per_commit,
        rename = opts.rename,
        ignore_merges = opts.ignore_merges,
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

mod git_input;

pub use git_input::{BlameSourceResolution, resolve_blame_source_files, run_git_diff};
pub(crate) use git_input::{GitDiffInput, resolve_git_diff};
#[cfg(test)]
pub(crate) use git_input::{is_git_work_tree, validate_git_revision};

#[allow(clippy::too_many_arguments)]
pub fn cmd_context(
    service: &AppService,
    dir: &str,
    diff: Option<&str>,
    diff_file: Option<&str>,
    git: bool,
    base: &str,
    staged: bool,
    pretty: bool,
    exclude_dirs: &[String],
    exclude_globs: &[String],
) -> Result<()> {
    let diff_input = if let Some(d) = diff {
        d.to_string()
    } else if let Some(df) = diff_file {
        read_file_to_string_limited(df, MAX_INPUT_SIZE)?
    } else if git {
        match resolve_git_diff(dir, base, staged)? {
            GitDiffInput::Diff(s) => s,
            GitDiffInput::Skipped(skip) => {
                // git 管理外: 空の changes + skipped を返して exit 0。
                let result = crate::models::impact::ContextResult {
                    changes: Vec::new(),
                    skipped: Some(skip),
                };
                println!("{}", serialize_output(&result, pretty)?);
                return Ok(());
            }
        }
    } else {
        let stdin = std::io::stdin();
        read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
    };

    let options = crate::models::impact::ContextAnalysisOptions {
        exclude_dirs: exclude_dirs.to_vec(),
        exclude_globs: exclude_globs.to_vec(),
    };

    if pretty {
        // pretty 出力は人間向けで整形が必要なため、従来どおり全 FileImpact を集約してから
        // 一括 serialize する。数 GB 級リポでは compact 出力推奨。
        let result = service.analyze_context(&diff_input, dir, &options)?;
        let output = serialize_output(&result, pretty)?;
        info!(
            command = "context",
            dir = dir,
            diff_bytes = diff_input.len(),
            output_bytes = output.len(),
            "command completed"
        );
        println!("{output}");
        return Ok(());
    }

    // compact 出力: streaming API で `FileImpact` を 1 件ずつ stdout に flush し、
    // `Vec<FileImpact>` の累積による数 GB 級ピーク RSS を排除する。
    use std::io::Write;
    service.validate_context_inputs(&diff_input, dir, &options)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(b"{\"changes\":[")?;
    let mut first = true;
    let mut changes_count = 0usize;
    service.analyze_context_streaming(&diff_input, dir, &options, |impact| {
        if !first {
            out.write_all(b",")
                .map_err(|e| anyhow::anyhow!("stdout write failed: {e}"))?;
        }
        serde_json::to_writer(&mut out, &impact)
            .map_err(|e| anyhow::anyhow!("json serialization failed: {e}"))?;
        first = false;
        changes_count += 1;
        Ok(())
    })?;
    out.write_all(b"]}\n")?;
    info!(
        command = "context",
        dir = dir,
        diff_bytes = diff_input.len(),
        changes = changes_count,
        "command completed (streaming)"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_impact(
    service: &AppService,
    dir: &str,
    git: bool,
    base: &str,
    staged: bool,
    hook: bool,
    exclude_dirs: &[String],
    exclude_globs: &[String],
) -> Result<()> {
    let diff_input = if git {
        match resolve_git_diff(dir, base, staged)? {
            GitDiffInput::Diff(s) => s,
            // git 管理外: 既存の「差分なし」と同じく無出力で exit 0。
            // impact は構造化 JSON 出力を持たず未解決 caller 検出時のみ stderr に
            // 出力する設計のため、skipped JSON は出さない (hook の有無を問わず silent)。
            GitDiffInput::Skipped(_) => return Ok(()),
        }
    } else {
        let stdin = std::io::stdin();
        read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
    };

    if diff_input.trim().is_empty() {
        return Ok(());
    }

    let options = crate::models::impact::ContextAnalysisOptions {
        exclude_dirs: exclude_dirs.to_vec(),
        exclude_globs: exclude_globs.to_vec(),
    };
    let result = service.analyze_context(&diff_input, dir, &options)?;

    // 変更されたファイルパスを事前に canonicalize してキャッシュ（O(M) syscall に削減）
    let changed_paths: std::collections::HashSet<&str> =
        result.changes.iter().map(|c| c.path.as_str()).collect();
    let changed_canonical: std::collections::HashSet<std::path::PathBuf> = changed_paths
        .iter()
        .filter_map(|cp| {
            let abs = if std::path::Path::new(cp).is_relative() {
                std::path::Path::new(dir).join(cp)
            } else {
                std::path::PathBuf::from(cp)
            };
            std::fs::canonicalize(&abs).ok()
        })
        .collect();
    // canonicalize 失敗時のフォールバック用に文字列セットも保持
    let changed_abs_strs: std::collections::HashSet<String> = changed_paths
        .iter()
        .map(|cp| {
            if std::path::Path::new(cp).is_relative() {
                std::path::Path::new(dir)
                    .join(cp)
                    .to_string_lossy()
                    .to_string()
            } else {
                cp.to_string()
            }
        })
        .collect();

    // 未解決の影響をグループ化: diff に含まれないファイルの caller
    // caller ごとに影響シンボルを追跡
    struct UnresolvedCaller {
        path: String,
        line: usize,
        symbols: Vec<String>,
    }
    let mut unresolved: std::collections::BTreeMap<String, Vec<UnresolvedCaller>> =
        std::collections::BTreeMap::new();

    for change in &result.changes {
        if change.affected_symbols.is_empty() {
            continue;
        }

        for caller in &change.impacted_callers {
            // 比較のため相対パスを dir を基準に解決
            let caller_abs = if std::path::Path::new(&caller.path).is_relative() {
                std::path::Path::new(dir)
                    .join(&caller.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                caller.path.clone()
            };

            // caller のファイルが変更ファイルに含まれていないかチェック（キャッシュ参照で O(1)）
            let in_diff = match std::fs::canonicalize(&caller_abs) {
                Ok(canon) => changed_canonical.contains(&canon),
                Err(_) => changed_abs_strs.contains(&caller_abs),
            };

            if !in_diff {
                unresolved
                    .entry(change.path.clone())
                    .or_default()
                    .push(UnresolvedCaller {
                        path: caller.path.clone(),
                        line: caller.line,
                        symbols: caller.symbols.clone(),
                    });
            }
        }
    }

    if unresolved.is_empty() {
        return Ok(());
    }

    eprintln!("Unresolved impacts found:\n");
    for (changed_path, callers) in &unresolved {
        // caller のシンボルを集約して表示用リストを作成
        let all_symbols: std::collections::BTreeSet<&str> = callers
            .iter()
            .flat_map(|c| c.symbols.iter().map(|s| s.as_str()))
            .collect();
        eprintln!(
            "{} changed [{}]:",
            changed_path,
            all_symbols.into_iter().collect::<Vec<_>>().join(", ")
        );
        for caller in callers {
            if caller.symbols.is_empty() {
                eprintln!("  → {}:{}", caller.path, caller.line);
            } else {
                eprintln!(
                    "  → {}:{} [{}]",
                    caller.path,
                    caller.line,
                    caller.symbols.join(", ")
                );
            }
        }
        eprintln!();
    }

    if hook {
        eprintln!(
            "IMPORTANT: If these look like false positives, you MUST run the `astro-sight-triage` skill to generate a triage report."
        );
    }

    std::process::exit(1);
}

pub fn cmd_doctor(pretty: bool) -> Result<()> {
    let report = doctor::run_doctor();
    let output = serialize_output(&report, pretty)?;
    info!(
        command = "doctor",
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

pub fn cmd_session() -> Result<()> {
    let service = AppService::from_env()?;
    crate::session::run_session(|req| handle_request(&service, req))
}

pub fn cmd_mcp() -> Result<()> {
    use rmcp::ServiceExt;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let server = crate::mcp::AstroSightServer::new();
        let router = server.into_router();
        let transport = rmcp::transport::io::stdio();
        let service = router
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP server initialization failed: {e}"))?;
        service
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Review コマンド: impact / cochange / API surface diff / dead symbol 統合
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn cmd_review(
    service: &AppService,
    dir: &str,
    diff: Option<&str>,
    diff_file: Option<&str>,
    git: bool,
    base: &str,
    staged: bool,
    min_confidence: f64,
    pretty: bool,
    hook: bool,
    framework: Option<&str>,
    extra_exclude_dirs: &[String],
    extra_exclude_globs: &[String],
    dead_scope: crate::cli::DeadScope,
    strict_public_const_values: bool,
    include_wip_dead: bool,
) -> Result<()> {
    // framework 指定は早期に検証して未知名はここで弾く (dead_symbols 検出に到達する前に)。
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    // 1. diff 取得（context コマンドと同じ入力方式）
    let diff_input = if let Some(d) = diff {
        d.to_string()
    } else if let Some(df) = diff_file {
        read_file_to_string_limited(df, MAX_INPUT_SIZE)?
    } else if git {
        match resolve_git_diff(dir, base, staged)? {
            GitDiffInput::Diff(s) => s,
            GitDiffInput::Skipped(skip) => {
                // git 管理外: hook は完全 silent、通常は空結果 + skipped で exit 0。
                if hook {
                    return Ok(());
                }
                let result = ReviewResult {
                    skipped: Some(skip),
                    ..Default::default()
                };
                let output = serialize_output(&result, pretty)?;
                println!("{output}");
                return Ok(());
            }
        }
    } else {
        let stdin = std::io::stdin();
        read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
    };

    if diff_input.trim().is_empty() {
        if hook {
            return Ok(());
        }

        let result = empty_review_result();
        let output = serialize_output(&result, pretty)?;
        println!("{output}");
        return Ok(());
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
    let force_ci = std::env::var("ASTRO_SIGHT_FORCE_CI_LANG_IMPACT")
        .ok()
        .as_deref()
        == Some("1");
    let all_ci_lang = crate::engine::impact::diff_files_all_case_insensitive(&diff_files);
    if all_ci_lang && !force_ci {
        log_phase("review.skip_ci_only", "applied", 0);
        if hook {
            return Ok(());
        }

        let result = empty_review_result();
        let output = serialize_output(&result, pretty)?;
        println!("{output}");
        return Ok(());
    }
    let impact = {
        log_phase("context", "start", 0);
        let phase_t = std::time::Instant::now();
        // review の `--exclude-dir` / `--exclude-glob` は impact 解析と dead_symbols の
        // 両方に作用させる (v26.5.117 で挙動を統一)。
        let context_options = crate::models::impact::ContextAnalysisOptions {
            exclude_dirs: extra_exclude_dirs.to_vec(),
            exclude_globs: extra_exclude_globs.to_vec(),
        };
        let r = service.analyze_context(&diff_input, dir, &context_options)?;
        log_phase("context", "end", phase_t.elapsed().as_millis());
        r
    };

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
    log_phase("cochange", "start", 0);
    let phase_t = std::time::Instant::now();
    let missing_cochanges =
        detect_missing_cochanges(service, dir, &changed_file_set, min_confidence, Some(base))?;
    log_phase("cochange", "end", phase_t.elapsed().as_millis());

    // 5. API 公開面の差分
    let api_changes = {
        log_phase("api_changes", "start", 0);
        let phase_t = std::time::Instant::now();
        let r = detect_api_changes(dir, base, &diff_files);
        log_phase("api_changes", "end", phase_t.elapsed().as_millis());
        r
    };

    // 6. dead symbol 検出 (framework プリセット + ユーザ指定 exclude を適用)
    //    review では vendor / tests / build を常に除外する固定挙動。
    //    必要になった段階で dead-code と同様の --include-* オプションを追加する。
    log_phase("dead_code", "start", 0);
    let phase_t = std::time::Instant::now();
    let (dead_symbols, test_only_symbols) = match std::fs::canonicalize(dir) {
        Ok(canonical_dir) => {
            let default_excludes = resolve_dead_code_excludes(false, false, false);
            let mut excludes: Vec<&str> = default_excludes.to_vec();
            for name in extra_exclude_dirs {
                excludes.push(name.as_str());
            }
            let mut combined_globs: Vec<&str> =
                framework_globs.iter().map(String::as_str).collect();
            for pat in extra_exclude_globs {
                combined_globs.push(pat.as_str());
            }
            let files = filter_diff_files_for_dead_code(
                &canonical_dir,
                &diff_files,
                &excludes,
                &combined_globs,
                None,
            )?;
            let (dead_symbols, test_only_symbols) = detect_dead_symbols_from_files(dir, &files);
            // dead-scope=touched-symbols: 宣言行が diff の `+` 行と重ならない dead を除外。
            // `--hook` のデフォルトで「changed file 内の元から存在した dead」の
            // ノイズを抑える (Issue: zod-inferred-types-pre-existing-dead)。
            let dead_symbols = if matches!(dead_scope, crate::cli::DeadScope::TouchedSymbols) {
                filter_dead_by_touched_symbols(dir, dead_symbols, &diff_input, &diff_files)
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
            let dead_symbols = if hook && !include_wip_dead {
                filter_dead_by_wip_added(dead_symbols, &api_changes.added)
            } else {
                dead_symbols
            };
            (dead_symbols, test_only_symbols)
        }
        Err(_) => (Vec::new(), Vec::new()),
    };
    log_phase("dead_code", "end", phase_t.elapsed().as_millis());

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

fn empty_review_result() -> ReviewResult {
    ReviewResult::default()
}

mod review;

#[cfg(test)]
pub(crate) use review::hook::build_review_hook_json;
pub(crate) use review::hook::review_hook_output;

mod api_changes;
mod dead_code;
mod dead_code_member_liveness;

#[cfg(test)]
pub(crate) use api_changes::*;
pub(crate) use api_changes::{detect_api_changes, detect_missing_cochanges};
pub use dead_code::cmd_dead_code;
#[cfg(test)]
pub(crate) use dead_code::{auto_detect_framework, extract_dead_code_candidates_from_file};
pub(crate) use dead_code::{
    detect_dead_symbols_from_files, filter_dead_by_touched_symbols, filter_dead_by_wip_added,
    filter_diff_files_for_dead_code, resolve_dead_code_excludes,
    resolve_framework_globs_with_auto_detect,
};

mod batch;
mod session_handler;

pub use batch::{batch_ast, batch_calls, batch_imports, batch_lint, batch_sequence, batch_symbols};
pub use session_handler::handle_request;

#[cfg(test)]
mod tests;
