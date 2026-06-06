use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::models::cochange::{CoChangeOptions, CoChangeResult};
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
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

/// refs --names のバッチ chunk サイズ。既定 64。
/// 大きいほどディレクトリ走査回数が減るが、Xojo の汎用名 (`row` / `e` 等) が
/// 大量ヒットすると find_references_batch が chunk 分の参照を同時保持して RSS が
/// 増えるため、既定は控えめにする。ASTRO_SIGHT_REFS_BATCH_CHUNK で上書き可能。
fn refs_batch_chunk_size() -> usize {
    std::env::var("ASTRO_SIGHT_REFS_BATCH_CHUNK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(64)
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

    // 名前を chunk 単位でまとめて検索する。1 名前ごとに呼び出すと
    // find_references_batch 内部のディレクトリウォーク + 全ファイル読込を名前数 N 回
    // 繰り返し O(N × ファイル数) に退化する (大規模リポでは数十分規模)。chunk で
    // まとめると走査回数が ceil(N / chunk) に減る。chunk を大きくしすぎると Xojo の
    // 汎用名 (`row` / `e` / `setting` 等) の大量ヒット時に find_references_batch が
    // `Vec<Vec<SymbolReference>>` を chunk 分保持して RSS が線形増大するため、既定は
    // 控えめ (64) にし env で上書き可能にする。service.find_references_batch は入力順を
    // 保った `Vec<RefsResult>` を返すため、chunk を names 順に処理すれば全体の NDJSON
    // 出力も names 順を維持する。
    for chunk in names.chunks(refs_batch_chunk_size()) {
        let results = service.find_references_batch(chunk, dir, glob)?;
        for result in &results {
            total_refs += result.references.len();
            let line = serde_json::to_string(result)?;
            writeln!(out, "{line}")?;
        }
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

#[cfg(test)]
pub(crate) use git_input::is_git_work_tree;
pub use git_input::{BlameSourceResolution, resolve_blame_source_files, run_git_diff};
pub(crate) use git_input::{GitDiffInput, resolve_git_diff, validate_git_revision};

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

/// 依存マニフェストとロックファイルの既知ペア。
/// これらは `cargo update` や `npm install` など片側のみが変更される正規操作が頻繁に発生するため、
/// missing_cochange 警告から除外する。同一ディレクトリに属するペアのみ除外対象とする（monorepo 配慮）。
const DEPENDENCY_MANIFEST_LOCK_PAIRS: &[(&str, &str)] = &[
    ("Cargo.toml", "Cargo.lock"),
    ("package.json", "package-lock.json"),
    ("package.json", "pnpm-lock.yaml"),
    ("package.json", "yarn.lock"),
    ("pyproject.toml", "uv.lock"),
    ("pyproject.toml", "poetry.lock"),
    ("pyproject.toml", "pdm.lock"),
    ("Gemfile", "Gemfile.lock"),
    ("composer.json", "composer.lock"),
    ("go.mod", "go.sum"),
    ("mix.exs", "mix.lock"),
];

/// 2 つのパスが既知の依存マニフェスト/ロックペアであれば true を返す。
/// monorepo 誤判定を避けるため、親ディレクトリが一致する場合のみ真。
fn is_dependency_manifest_pair(file_a: &str, file_b: &str) -> bool {
    let path_a = std::path::Path::new(file_a);
    let path_b = std::path::Path::new(file_b);
    let (Some(base_a), Some(base_b)) = (
        path_a.file_name().and_then(|s| s.to_str()),
        path_b.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    if path_a.parent() != path_b.parent() {
        return false;
    }
    DEPENDENCY_MANIFEST_LOCK_PAIRS
        .iter()
        .any(|(a, b)| (base_a == *a && base_b == *b) || (base_a == *b && base_b == *a))
}

fn detect_missing_cochanges(
    service: &AppService,
    dir: &str,
    changed_files: &HashSet<String>,
    min_confidence: f64,
    base: Option<&str>,
) -> Result<Vec<MissingCochange>> {
    // review では blame モードで cochange を解析する。
    // 起点ファイル = 差分に登場したファイル。
    // ただし起点が無い (差分が空) ときは何もせず空を返す。
    let source_files: Vec<String> = changed_files.iter().cloned().collect();
    if source_files.is_empty() {
        return Ok(Vec::new());
    }
    // review の差分取得で使った base を blame 解析にも渡し、複数コミット範囲の
    // review でも同じ変更範囲を対象にする。base 解決失敗や git 不在は engine 側で
    // 空集合を返すので最終的に Vec::new() に落ちる。
    let opts = CoChangeOptions {
        source_files,
        base: base.map(str::to_string),
        min_confidence,
        ..CoChangeOptions::default()
    };
    let cochange_result = match service.analyze_cochange(dir, &opts) {
        Ok(r) => r,
        Err(err) => {
            // 入力検証エラー (min_confidence の NaN / 範囲外等) はユーザーへ伝播する。
            // git 不在 / base 解決失敗は engine 側で empty 結果を返すため、ここまで
            // Err が来ない。InvalidRequest だけ早期失敗させて silent な誤動作を防ぐ。
            if let Some(astro_err) = err.downcast_ref::<crate::error::AstroError>()
                && astro_err.code == crate::error::ErrorCode::InvalidRequest
            {
                return Err(err);
            }
            return Ok(Vec::new());
        }
    };

    // 各 missing file につき最も confidence が高いペアのみ残す
    let mut best: std::collections::HashMap<String, MissingCochange> =
        std::collections::HashMap::new();
    for entry in &cochange_result.entries {
        // 依存マニフェスト/ロックペアは片側変更が正規操作として頻発するためスキップ
        if is_dependency_manifest_pair(&entry.file_a, &entry.file_b) {
            continue;
        }

        let a_in_diff = changed_files.contains(&entry.file_a);
        let b_in_diff = changed_files.contains(&entry.file_b);

        let candidate = if a_in_diff && !b_in_diff {
            Some(MissingCochange {
                file: entry.file_b.clone(),
                expected_with: entry.file_a.clone(),
                confidence: entry.confidence,
            })
        } else if b_in_diff && !a_in_diff {
            Some(MissingCochange {
                file: entry.file_a.clone(),
                expected_with: entry.file_b.clone(),
                confidence: entry.confidence,
            })
        } else {
            None
        };

        if let Some(c) = candidate {
            best.entry(c.file.clone())
                .and_modify(|existing| {
                    if c.confidence > existing.confidence {
                        *existing = c.clone();
                    }
                })
                .or_insert(c);
        }
    }

    // confidence 降順でソートし最大10件に制限
    let mut missing: Vec<MissingCochange> = best.into_values().collect();
    missing.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    missing.truncate(10);
    Ok(missing)
}

/// 内部用: reconcile のために signature を保持する一時構造。
#[derive(Debug, Clone)]
struct ApiSymbolCandidate {
    name: String,
    kind: String,
    file: String,
    signature: String,
}

impl ApiSymbolCandidate {
    fn into_api_symbol(self) -> ApiSymbol {
        ApiSymbol {
            name: self.name,
            kind: self.kind,
            file: self.file,
        }
    }
}

fn detect_api_changes(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> ApiChanges {
    let mut added: Vec<ApiSymbolCandidate> = Vec::new();
    let mut removed: Vec<ApiSymbolCandidate> = Vec::new();
    let mut modified: Vec<ApiSymbolChange> = Vec::new();
    // 全 cross-file 参照が同一 diff 内で追随済みの api.mod (informational)。
    let mut modified_closed_in_diff: Vec<ApiSymbolChange> = Vec::new();
    // const / 非 mut static / export const の値 (initializer) のみ変更で shape (名前・型・
    // visibility) は不変な api.mod。コンパイル互換性を壊さないため別カテゴリに分離する
    // (Issue 2026-06-02-balance-const-value-changes 対応)。
    let mut const_value_changes: Vec<ApiSymbolChange> = Vec::new();
    // 互換性ありと判定された api.mod (React HOC ラップ / 未参照プロパティ削除)。非 blocking。
    let mut compatible_modified: Vec<CompatibleApiModification> = Vec::new();
    // 移動検出用に「フィルタ前の新規側候補」を全件追跡する。`is_used_in_diff_paths` 等で
    // `added` から除外された候補も `removed` との突き合わせには利用したいため、
    // フィルタ適用前の候補を別バケットに溜めておく。module → package 化 (cli.py →
    // cli/__init__.py + cli/_commands/*.py) のように、新規ファイル側のシンボルが
    // 同 diff 内の別ファイルから参照されて `added` から消える典型ケースで、対応する
    // `removed` を `moved` として相殺するために使う。
    let mut all_new_candidates: Vec<ApiSymbolCandidate> = Vec::new();
    // Python の `@property def x(self)` を `@dataclass` フィールド `x: T` に置き換えた
    // ケースを informational として残す。`obj.x` の属性 API は維持されるため `removed`
    // からは除外し、`property_to_field` カテゴリに移す。
    let mut property_to_field: Vec<PropertyToFieldChange> = Vec::new();
    // api.rm の Rust private module 抑制で base 側 re-export index を base+crate 単位に再利用する。
    let mut rust_reexport_cache = RustBaseReexportCache::default();
    // api.add 経路では new (working tree) 側 crate を 1 度走査して edge graph を構築する。
    let mut rust_new_reexport_cache = RustWorktreeReexportCache::default();

    // .gitattributes の linguist-generated 指定ファイルは API 変更検出から除外する
    let gitattrs = std::fs::canonicalize(dir)
        .map(|d| crate::engine::gitattributes::GitAttributes::load(&d))
        .unwrap_or_default();

    // 同一 diff 内で追加/変更されたファイルパスの集合。新規 pub シンボルが diff 内の
    // 別ファイルから参照されていれば「同一 diff 内で完結して使用されている」と判断し、
    // api.add から除外する（binary crate の pub struct が同 diff 内で use されるケース等）。
    let diff_new_paths: HashSet<String> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        .map(|f| f.new_path.clone())
        .collect();

    let canonical_dir = std::fs::canonicalize(dir).ok();
    for df in diff_files {
        // 信頼境界外の diff パスを `dir.join()` で読まないよう、絶対パス・トラバーサル
        // を含むパスはここで弾く。下流の `extract_exported_symbols_from_file` でも
        // 多層防御として再チェックしている。
        if df.new_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.new_path) {
            continue;
        }
        if df.old_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.old_path) {
            continue;
        }
        // 新旧いずれかが生成物扱いなら API 変更検出の対象外
        if gitattrs.is_generated(&df.new_path) || gitattrs.is_generated(&df.old_path) {
            continue;
        }
        // ファイル先頭の自動生成マーカーコメントでも除外する
        if let Some(root) = &canonical_dir
            && df.new_path != "/dev/null"
        {
            let full = root.join(&df.new_path);
            if crate::engine::generated::is_auto_generated(&full) {
                continue;
            }
        }

        if df.old_path == "/dev/null" {
            if let Some(new_syms) = extract_exported_symbols_from_file(dir, &df.new_path) {
                // bin-only crate (src/lib.rs なし) の pub シンボルは crate 外から構造的に
                // 到達できないため api.add 対象外。private module 抑制は symbol 単位 (loop 内)。
                let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);
                let in_file_callees = extract_in_file_callees(dir, &df.new_path);
                for (name, kind, sig) in &new_syms {
                    let candidate = ApiSymbolCandidate {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: df.new_path.clone(),
                        signature: sig.clone(),
                    };
                    // 移動検出には全候補を必要とするので、フィルタ前に積む。
                    all_new_candidates.push(candidate.clone());
                    if is_binary_rust_crate {
                        continue;
                    }
                    // private module 配下でも、別の public-reachable module から `pub use` で
                    // re-export 公開されているシンボルは api.add 対象として残す (Issue
                    // 2026-06-05-rust-api-add-private-module-reexport-edge-graph 対応)。
                    // api.rm / api.mod と同じ edge graph + 固定点伝播で判定する。
                    if is_rust_new_symbol_outside_public_api_surface(
                        dir,
                        &df.new_path,
                        name,
                        &mut rust_new_reexport_cache,
                    ) {
                        continue;
                    }
                    if is_internally_connected(&in_file_callees, name) {
                        continue;
                    }
                    // 同一 diff 内の別ファイルから参照されている新規シンボルは
                    // 「コミット内で完結」として api.add から除外する。
                    if is_used_in_diff_paths(dir, name, &df.new_path, &diff_new_paths) {
                        continue;
                    }
                    added.push(candidate);
                }
            }
            continue;
        }

        if df.new_path == "/dev/null" {
            // base が source branch HEAD と同一 (例: CI が source branch を checkout した直後で
            // base コミット引数も HEAD のまま) の場合、`git show base:old_path` は削除済みで
            // 失敗し None になる。その場合は --diff-file が保持している旧ソース (deleted_old_source)
            // から AST を組み立てて exported シンボルを抽出するフォールバックを試す。
            let old_syms_opt =
                extract_exported_symbols_from_git(dir, base, &df.old_path).or_else(|| {
                    df.deleted_old_source
                        .as_deref()
                        .and_then(|src| extract_exported_symbols_from_source(&df.old_path, src))
                });
            if let Some(old_syms) = old_syms_opt {
                // Bash ファイル丸ごと削除のケース（CLI スクリプトを別言語に書き換え等）でも
                // 未 export 関数は外部 API 面ではない。新ツリー全体で参照 0 件なら同一 diff
                // 内で完結した削除と判断して api.rm から除外する。
                let is_bash_old_file = is_bash_script_path(&df.old_path);
                // Rust bin-only crate (`[[bin]]` のみで `[lib]` なし) の `pub fn`、および
                // crate-private module (`mod foo`、`pub mod` 経路で到達不能) 配下の `pub fn` は
                // クレート外から構造的に到達できないため、削除されても外部 API の破壊にはならない。
                // `api.add` (new 側) / `api.mod` (old/new 両側) の private module 抑制と対称に
                // `api.rm` 側でも除外する。`api.rm` は旧 API 面の判定なので base リビジョン側で
                // 見る (新ツリーで src/lib.rs や mod 宣言を削除したケースでも誤抑制しないため)。
                for (name, kind, sig) in &old_syms {
                    if is_rust_old_symbol_outside_public_api_surface(
                        dir,
                        base,
                        &df.old_path,
                        name,
                        &mut rust_reexport_cache,
                    ) {
                        continue;
                    }
                    if is_bash_old_file
                        && !bash_function_is_exported_in_git(dir, base, &df.old_path, name)
                        && is_removed_bash_symbol_unreferenced(dir, name)
                    {
                        continue;
                    }
                    // Python の @property → dataclass field 置き換えなら removed
                    // 扱いせず property_to_field に振り替える。
                    if let Some(target_file) =
                        detect_python_property_to_field(dir, name, &diff_new_paths)
                    {
                        property_to_field.push(PropertyToFieldChange {
                            name: name.clone(),
                            file: target_file,
                        });
                        continue;
                    }
                    removed.push(ApiSymbolCandidate {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: df.old_path.clone(),
                        signature: sig.clone(),
                    });
                }
            }
            continue;
        }

        // rename 差分では base 側に新パスが存在しないため、旧版は old_path から読む。
        let old_syms = extract_exported_symbols_from_git(dir, base, &df.old_path);
        let new_syms = extract_exported_symbols_from_file(dir, &df.new_path);

        let (old_syms, new_syms) = match (old_syms, new_syms) {
            (Some(o), Some(n)) => (o, n),
            _ => continue,
        };

        let old_map: std::collections::HashMap<&str, &str> = old_syms
            .iter()
            .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
            .collect();
        let new_map: std::collections::HashMap<&str, (&str, &str)> = new_syms
            .iter()
            .map(|(name, kind, sig)| (name.as_str(), (kind.as_str(), sig.as_str())))
            .collect();

        // 同名シンボルが旧/新いずれかに複数存在する場合、HashMap<name, sig> は最後の 1 件しか
        // 保持できず、別のオーバーロードや誤パースされた定義同士を突き合わせて api.mod を
        // 誤検出する。出現回数を数え、複数あるシンボルは曖昧として modified 判定から除外する
        // (Issue #13: C++ overload / マクロ誤パースの api.mod 誤検出対策)。
        let mut old_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in &old_syms {
            *old_name_counts.entry(name.as_str()).or_default() += 1;
        }
        let mut new_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in &new_syms {
            *new_name_counts.entry(name.as_str()).or_default() += 1;
        }

        // 新ファイル内の call 先名を集める。同一ファイル内から呼ばれている新規関数は
        // 「内部ヘルパー」として api.add から除外する（Bash スクリプトのトップレベル関数や
        // Python の同一ファイル内で接続済みの private 関数が api.add に出る誤検出対策）。
        let in_file_callees = extract_in_file_callees(dir, &df.new_path);

        // bin-only crate (src/lib.rs なし) の pub シンボルは外部到達不能のため api.add 対象外。
        // private module 抑制は symbol 単位 (loop 内) で edge graph 判定する。
        let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);

        // rename 検出用: 同ファイル内に新規追加された全シンボル名を追跡する
        // （internally_connected で除外される内部ヘルパーも含む）。削除シンボルと
        // 組み合わせて「rename + 実装置換」の api.rm ノイズを抑止する。
        let mut new_symbols_in_current_file: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for (name, kind, sig) in &new_syms {
            if !old_map.contains_key(name.as_str()) {
                new_symbols_in_current_file.insert(name.clone());
                let candidate = ApiSymbolCandidate {
                    name: name.clone(),
                    kind: kind.clone(),
                    file: df.new_path.clone(),
                    signature: sig.clone(),
                };
                // 移動検出には全候補を必要とするので、フィルタ前に積む。
                all_new_candidates.push(candidate.clone());
                if is_binary_rust_crate {
                    continue;
                }
                // private module 配下でも別 public module 経由 re-export なら api.add に残す。
                if is_rust_new_symbol_outside_public_api_surface(
                    dir,
                    &df.new_path,
                    name,
                    &mut rust_new_reexport_cache,
                ) {
                    continue;
                }
                // qualname または bare name が同一ファイル内の call に存在すれば内部参照
                if is_internally_connected(&in_file_callees, name) {
                    continue;
                }
                // 同一 diff 内の別ファイルから参照されている新規シンボルは
                // 「コミット内で完結」として api.add から除外する（pub struct の import 等）。
                if is_used_in_diff_paths(dir, name, &df.new_path, &diff_new_paths) {
                    continue;
                }
                added.push(candidate);
            }
        }

        // Bash スクリプトでは関数定義は `export -f` (または `declare -fx`/`declare -xf`) で
        // 明示しない限りサブプロセスへ波及しない。CLI スクリプト内のローカルヘルパーが
        // 同一 diff 内で削除されたとき、純粋な関数削除（同ファイルに新規追加なし）でも
        // api.rm から除外できるよう、bash ファイルかつ未 export 関数のときは closed-in-diff
        // ロジックを純粋削除にも拡張する。`export -f` 済み関数は他リポジトリ消費者向け
        // API として残す。
        let is_bash_old_file = is_bash_script_path(&df.old_path);
        // Rust bin-only crate (`[[bin]]` のみで `[lib]` なし) の `pub fn`、および crate-private
        // module (`mod foo`) 配下の `pub fn` は外部から到達できないため、削除されても破壊的
        // API 変更にはならない。`api.add` / `api.mod` 側と対称に api.rm でも除外する
        // (Issue 2026-05-19-api-rm-bin-crate-dead-cleanup / 2026-06-05-wifi-module-removal 対応)。
        // `api.rm` は旧 API 面の判定なので、`base` リビジョン時点で見る。
        // TS/JS: 新ツリーで `export { name } from "..."` として re-export (forwarding)
        // されているシンボルは、利用者から見た API 面 (import path から取れる名前) が
        // 維持されているため api.rm から除外する。ローカル定義を別モジュールへ移動し
        // re-export を残すリファクタの誤検出対策。`export * from` は名前不明のため対象外。
        let new_reexports = extract_reexported_names_from_file(dir, &df.new_path);
        for (name, kind, sig) in &old_syms {
            if !new_map.contains_key(name.as_str()) {
                if is_rust_old_symbol_outside_public_api_surface(
                    dir,
                    base,
                    &df.old_path,
                    name,
                    &mut rust_reexport_cache,
                ) {
                    continue;
                }
                if new_reexports.contains(name.as_str()) {
                    continue;
                }
                // closed-in-diff for api.rm: 同ファイルに新規追加されたシンボルがあり、
                // 削除されたシンボルが変更後ツリーで 0 件参照なら「rename + 実装置換」
                // と判断して api.rm から除外する。caller は同一 diff 内で追随済み。
                // 純粋な関数削除（新規追加がない）は api.rm に残す。
                let bash_pure_removal_skip = is_bash_old_file
                    && new_symbols_in_current_file.is_empty()
                    && !bash_function_is_exported_in_git(dir, base, &df.old_path, name);
                if (!new_symbols_in_current_file.is_empty() || bash_pure_removal_skip)
                    && is_removed_symbol_unreferenced(dir, name)
                {
                    continue;
                }
                // Python の @property → dataclass field 置き換えなら removed 扱い
                // せず property_to_field に振り替える。
                if let Some(target_file) =
                    detect_python_property_to_field(dir, name, &diff_new_paths)
                {
                    property_to_field.push(PropertyToFieldChange {
                        name: name.clone(),
                        file: target_file,
                    });
                    continue;
                }
                removed.push(ApiSymbolCandidate {
                    name: name.clone(),
                    kind: kind.clone(),
                    // 削除シンボルの出所は旧ファイルパス
                    file: df.old_path.clone(),
                    signature: sig.clone(),
                });
            }
        }

        // Rust bin-only crate (`[[bin]]` のみで `[lib]` も `src/lib.rs` も持たない) の
        // `pub fn` シグネチャ変更は外部公開 API の互換性問題ではなく内部リファクタ。
        // `api.add` / `api.rm` 側と対称に `api.mod` 側でも除外する
        // (Issue 2026-05-20-api-mod-callers-updated-in-same-commit 対応)。
        //
        // 同コミットで lib → bin / bin → lib に crate type を変えた edge ケースは
        // どちらかが bin-only と判定された時点で「外部 API 面の変更ではない」と扱う方が
        // false positive を減らせる (codex 設計相談で「old または new のどちらかが
        // bin-only なら api.mod から除外」が筋良いと判定)。
        let is_binary_rust_old_crate_for_mod =
            is_binary_only_rust_crate_at_base(dir, base, &df.old_path);
        let is_binary_rust_new_crate_for_mod = is_binary_only_rust_crate(dir, &df.new_path);
        // bin-only crate (`[[bin]]` のみで `[lib]` も `src/lib.rs` も持たない) の `pub fn`
        // シグネチャ変更は外部公開 API の互換性問題ではない。ファイル単位で skip を確定できる。
        let skip_mod_for_binary_crate =
            is_binary_rust_old_crate_for_mod || is_binary_rust_new_crate_for_mod;

        // 値バインディングの value-only 変更を const_value_changes へ振り分けるための言語判定。
        let lang_id_for_file =
            crate::language::LangId::from_path(camino::Utf8Path::new(df.new_path.as_str())).ok();

        // 同一 (file, qualname) の modified を重複排除するためのキーセット
        let mut seen_modified: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for (name, kind, new_sig) in &new_syms {
            if let Some(old_sig) = old_map.get(name.as_str())
                && old_sig != &new_sig.as_str()
                && seen_modified.insert((df.new_path.clone(), name.clone()))
            {
                if skip_mod_for_binary_crate {
                    continue;
                }
                // private module 抑制: api.rm と同じ re-export edge graph + 固定点伝播で、
                // 公開到達不能と確定したシンボルのみ api.mod から除外する (codex Warning #4 対応)。
                // 二段 re-export 経由で外部公開されているシンボルの破壊的 signature 変更は
                // blocking を維持する。fail-closed: index 構築失敗時は除外せず modified に残す。
                if is_rust_old_symbol_outside_public_api_surface(
                    dir,
                    base,
                    &df.old_path,
                    name,
                    &mut rust_reexport_cache,
                ) {
                    continue;
                }
                // 同名が旧/新いずれかに複数あるシンボルは、別の定義同士を突き合わせている
                // 可能性があり曖昧なので modified から除外する (Issue #13)。
                if old_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
                    || new_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
                {
                    continue;
                }
                // closed-in-diff: 同一ファイル内でしか呼ばれていない関数のシグネチャ変更は
                // caller の追随が同一 diff 内で完結するため、api.mod から除外する。
                // bash エントリポイントのローカル関数や Python CLI スクリプト内部関数の
                // シグネチャ変更がレビューノイズになる問題への対策。
                // added 側の `is_internally_connected` フィルタと対称。
                if is_internally_connected(&in_file_callees, name)
                    && !has_cross_file_refs(dir, &df.new_path, name)
                {
                    continue;
                }

                // TS/TSX で「引数なし `()` → 省略可能 destructured 引数 `({}: T)` /
                // `({}: T = {})` 追加」は呼び出し側 (`foo()` / `<Foo />`) に影響しない
                // 後方互換変更のため api.mod から除外する
                // (Issue 2026-05-28-meet-virtual-you-frontend-modernize 対応)。
                if is_ts_no_arg_to_optional_destructured_compatible(
                    old_sig,
                    new_sig,
                    dir,
                    base,
                    &df.old_path,
                    &df.new_path,
                    name,
                ) {
                    continue;
                }

                let change = ApiSymbolChange {
                    name: name.clone(),
                    kind: kind.clone(),
                    file: df.new_path.clone(),
                    old_signature: Some(old_sig.to_string()),
                    new_signature: Some(new_sig.clone()),
                };
                // const / 非 mut static / export const の値 (initializer) のみ変更で shape
                // (名前・型・visibility) が不変なら、コンパイル互換性を壊さないため
                // const_value_changes (informational) に振り分ける。
                if lang_id_for_file
                    .is_some_and(|lid| is_const_value_only_change(old_sig, new_sig, kind, lid))
                {
                    const_value_changes.push(change);
                }
                // React component を `memo` / `forwardRef` 等の HOC でラップしただけで
                // export 名・props 型・JSX 利用互換性が維持されるケースは、公開契約が
                // 変わらないため compatible_modified (informational) に降格する。
                else if let Some(compat) = detect_react_wrapper_compatible_mod(
                    dir,
                    base,
                    &df.old_path,
                    &df.new_path,
                    name,
                    kind,
                    old_sig,
                    new_sig,
                    lang_id_for_file,
                ) {
                    compatible_modified.push(compat);
                }
                // exported object のプロパティ削除で、削除されたメンバーへの member-access が
                // repo 全体で 0 件なら破壊的でないため compatible に降格する。
                else if let Some(compat) = detect_object_members_compatible_mod(
                    dir,
                    base,
                    &df.old_path,
                    &df.new_path,
                    name,
                    kind,
                    old_sig,
                    new_sig,
                    lang_id_for_file,
                ) {
                    compatible_modified.push(compat);
                }
                // 全 cross-file 参照が同一 diff 内の変更 hunk で追随済みなら、呼び出し側は
                // 同一コミットで更新済み。破壊的でないため informational に降格する。
                else if is_modified_closed_in_diff(dir, name, base, diff_files) {
                    modified_closed_in_diff.push(change);
                } else {
                    modified.push(change);
                }
            }
        }
    }

    // git の rename detection が効かない diff (外部供給 / 非 git 入力 / 設定で無効化された
    // 環境など) に対するフォールバックとして、同一 (name, kind, signature) の add/rm ペアを
    // rename または move として相殺し、`moved` カテゴリに移す。`all_new_candidates` には
    // `is_used_in_diff_paths` 等で `added` から外れた候補も含まれるため、module → package
    // 化のように新規ファイル側のシンボルが同 diff 内の `__init__.py` 等から参照されて
    // `added` に乗らないケースでも `removed` を相殺できる。
    let (added, removed, moved) = reconcile_with_moves(added, removed, all_new_candidates);

    // removed のうち HEAD ツリーで他ファイル参照 0 件のものを `removed_dead` に振り分け。
    // 「base 時点で dead だった symbol の整理」だけでなく「base alive → HEAD で関連
    // caller も削除」も同 diff 内で repo 内到達性 0 になるため同一カテゴリに含む。
    // 順序は moved > removed_dead (rename/move 相殺を先に行わないと移動が dead 誤分類
    // される)。codex 設計合意 (Issue
    // 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
    //
    // qualname (`Container.method`) は refs 検索が identifier ノードでマッチするため
    // 常に 0 件返却となり誤分類するため、bare name で検索する。同名 def が複数残って
    // いる場合は「部分的削除」or「同名複数定義」の可能性があるため保守的に removed
    // に残す (codex 指摘 1 対応)。
    //
    // 複数候補がある場合、`find_references_batch` で 1 度のリポジトリ走査に集約する
    // (codex 指摘 3 対応: 候補数 × リポ全体走査の回避)。
    let (removed_kept, removed_dead) = partition_removed_dead_candidates(dir, removed);

    ApiChanges {
        added: added.into_iter().map(|c| c.into_api_symbol()).collect(),
        removed: removed_kept
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified,
        moved,
        property_to_field,
        removed_dead: removed_dead
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified_closed_in_diff,
        const_value_changes,
        compatible_modified,
    }
}

/// TS/TSX/JS の exported component を `memo` / `forwardRef` 等の HOC でラップしただけの
/// api.mod を互換変更 (`react_component_wrapper`) として判定する。
///
/// `export function X(props: T) {}` → `export const X = memo(function X(props: T) {})` の
/// ように宣言種別が変わると signature 文字列が変化して api.mod になるが、export 名・props
/// 型・JSX 利用互換性が維持されるなら公開契約は不変。次をすべて満たすとき降格する:
/// - 言語が TS / TSX / JS
/// - new 側が `memo` / `forwardRef` (`React.*` 含む) でラップされている
/// - old / new 双方から `function <name>(<params>)` の引数リストを抽出でき正規化一致する
/// - 引数に型注釈がある (型なしは JSX 互換を保証できないため除外)
/// - 当該シンボルに値利用参照 (`X(...)` / `new X` / `typeof X` / `X.foo` / `X[...]`) が無い
///
/// 抽出失敗・型注釈なし・参照解析失敗・判定不能な参照は None を返し blocking を維持する
/// (false negative 回避)。
#[allow(clippy::too_many_arguments)]
fn detect_react_wrapper_compatible_mod(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    let lang =
        lang_id.filter(|l| matches!(l, LangId::Typescript | LangId::Tsx | LangId::Javascript))?;
    // new 側が memo / forwardRef でラップされていること (単なる function 本体変更は対象外)。
    if !new_sig_has_react_wrapper(new_sig) {
        return None;
    }
    // old 側は非 wrapper (function 宣言等) であること。wrapper-to-wrapper の変更
    // (`forwardRef<HTMLDivElement, P>` → `forwardRef<HTMLButtonElement, P>` 等) は ref 型や
    // generic の差分を取りこぼすため対象外 (codex 指摘)。
    if new_sig_has_react_wrapper(old_sig) {
        return None;
    }
    // 信頼境界外のパスは多層防御で再チェックする。
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    // old は base リビジョン、new は working tree からソースを再取得して props 型を AST 抽出
    // する。signature 文字列は const の先頭行 fallback で複数行 destructured props の型注釈を
    // 取りこぼすため、ソース再パースで比較する (codex 設計合意)。
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(old_path, "diff file path").ok()?;
    let old_output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{old_path}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !old_output.status.success() {
        return None;
    }
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new_source = parser::read_file(new_utf8).ok()?;
    // old / new 双方の第1引数 (props) の型注釈を抽出して一致を要求する。
    let old_props = extract_component_props_type(&old_output.stdout, lang, name)?;
    let new_props = extract_component_props_type(&new_source, lang, name)?;
    if old_props != new_props {
        return None;
    }
    // 値利用 (呼び出し / typeof / member / new / indexed) が残れば MemoExoticComponent 化で
    // 壊れ得るため blocking 維持。
    if has_blocking_value_usage(dir, name) {
        return None;
    }
    Some(CompatibleApiModification {
        name: name.to_string(),
        kind: kind.to_string(),
        file: new_path.to_string(),
        old_signature: Some(old_sig.to_string()),
        new_signature: Some(new_sig.to_string()),
        reason: "react_component_wrapper".to_string(),
    })
}

/// new 側 signature が `memo(` / `forwardRef(` / `React.memo(` / `React.forwardRef(` で
/// ラップされているか (identifier 境界を確認し `somememo` 等の部分一致を弾く)。
fn new_sig_has_react_wrapper(sig: &str) -> bool {
    let bytes = sig.as_bytes();
    for kw in ["memo", "forwardRef"] {
        let kb = kw.as_bytes();
        let mut i = 0;
        while i + kb.len() <= bytes.len() {
            if &bytes[i..i + kb.len()] == kb {
                let before_ok = i == 0 || {
                    let p = bytes[i - 1];
                    // `React.memo` の `.` は許容、識別子継続文字は不可
                    !(p.is_ascii_alphanumeric() || p == b'_' || p == b'$')
                };
                let after = sig[i + kb.len()..].trim_start();
                if before_ok && after.starts_with('(') {
                    return true;
                }
            }
            i += 1;
        }
    }
    false
}

/// TS/TSX/JS ソースから、トップレベル exported な `name` のコンポーネント関数の第1引数
/// (props) の型注釈テキスト (例 `: ScheduleItemProps`、whitespace 正規化済み) を抽出する。
/// `export function name(p: T)` / `export const name = memo(function(p: T))` /
/// `forwardRef((p: T, ref) => ...)` に対応し、宣言 subtree の最初の formal_parameters を見る。
/// 宣言が見つからない / 同名宣言が複数 / 第1引数に型注釈が無い / parse 失敗なら None
/// (呼び出し側で blocking 維持)。
fn extract_component_props_type(
    source: &[u8],
    lang_id: crate::language::LangId,
    name: &str,
) -> Option<String> {
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    let decls = find_toplevel_decls_named(root, name, source);
    if decls.len() != 1 {
        return None;
    }
    let params = first_descendant_formal_parameters(decls[0])?;
    first_param_type_text(params, source)
}

/// program 直下 (export_statement のラップを潜る) で `name` を宣言する function_declaration
/// または variable_declarator ノードを集める。
fn find_toplevel_decls_named<'a>(
    root: tree_sitter::Node<'a>,
    name: &str,
    source: &[u8],
) -> Vec<tree_sitter::Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let decl = if child.kind() == "export_statement" {
            match child.named_child(0) {
                Some(d) => d,
                None => continue,
            }
        } else {
            child
        };
        match decl.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if node_field_name_eq(decl, name, source) {
                    result.push(decl);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut c2 = decl.walk();
                for d in decl.named_children(&mut c2) {
                    if d.kind() == "variable_declarator" && node_field_name_eq(d, name, source) {
                        result.push(d);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// ノードの `name` フィールドのテキストが `name` と一致するか。
fn node_field_name_eq(node: tree_sitter::Node, name: &str, source: &[u8]) -> bool {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        == Some(name)
}

/// `node` の subtree を深さ優先で走査し最初の formal_parameters ノードを返す。
fn first_descendant_formal_parameters(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if node.kind() == "formal_parameters" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_descendant_formal_parameters(child) {
            return Some(found);
        }
    }
    None
}

/// formal_parameters の第1引数の型注釈テキスト (whitespace 正規化済み) を返す。
/// 第1引数が required/optional_parameter で `type` フィールドを持つときのみ Some。
/// 型注釈が無い (JS 風 identifier param 等) / 引数なしなら None。
fn first_param_type_text(params: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = params.walk();
    let first = params.named_children(&mut cursor).next()?;
    match first.kind() {
        "required_parameter" | "optional_parameter" => {
            let type_node = first.child_by_field_name("type")?;
            let text = type_node.utf8_text(source).ok()?;
            Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        _ => None,
    }
}

/// `name` シンボルが値として利用 (`X(...)` / `new X` / `typeof X` / `X.foo` / `X[...]`)
/// されている参照があるかを判定する。JSX タグ利用・import/re-export・定義のみなら false
/// (= 降格可)。解析失敗・判定不能な参照があれば true (= blocking 維持、false negative 回避)。
fn has_blocking_value_usage(dir: &str, name: &str) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let refs = match crate::engine::refs::find_references(bare, std::path::Path::new(dir), None) {
        Ok(r) => r,
        Err(_) => return true,
    };
    for r in &refs {
        if r.kind == Some(RefKind::Definition) {
            continue;
        }
        let Some(ctx) = r.context.as_deref() else {
            return true;
        };
        let trimmed = ctx.trim_start();
        // import / re-export specifier は値利用ではない
        if ref_is_import_line(r)
            || trimmed.starts_with("export {")
            || trimmed.starts_with("export type")
        {
            continue;
        }
        if !ctx_usage_is_jsx_or_safe(ctx, bare) {
            return true;
        }
    }
    false
}

/// 参照行 `ctx` 内の `name` 出現がすべて JSX タグ利用 (`<X` / `</X`) かを判定する。
/// 値利用 (`X(` 呼び出し / `X.` / `X[` / `new X` / `typeof X`) や JSX でない裸の出現を
/// 含むなら false (= blocking 側に倒す)。
fn ctx_usage_is_jsx_or_safe(ctx: &str, name: &str) -> bool {
    let bytes = ctx.as_bytes();
    let nb = name.as_bytes();
    if nb.is_empty() {
        return false;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = 0;
    let mut saw_occurrence = false;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            let before = if i == 0 { None } else { Some(bytes[i - 1]) };
            let after = bytes.get(i + nb.len()).copied();
            let before_boundary = before.is_none_or(|b| !is_ident(b));
            let after_boundary = after.is_none_or(|b| !is_ident(b));
            if before_boundary && after_boundary {
                saw_occurrence = true;
                let next_non_ws = ctx[i + nb.len()..].trim_start().as_bytes().first().copied();
                let is_call = next_non_ws == Some(b'(');
                let is_member = next_non_ws == Some(b'.') || next_non_ws == Some(b'[');
                // 直前の識別子トークンを取る (空白だけでなく `(` `=` 等の非識別子文字でも
                // 区切る)。`memo(function NAME` のように `(` 直後に関数キーワードが来るケースを
                // 正しく拾うため split_whitespace ではなく識別子境界で分割する。
                let last_ident = ctx[..i]
                    .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                    .find(|s| !s.is_empty())
                    .unwrap_or("");
                let is_typeof = last_ident == "typeof";
                let is_new = last_ident == "new";
                // 宣言キーワード直後の出現は定義 (変数宣言名 / named function expression 名 /
                // class 名) であり値利用でない。`export const X = memo(function X(...))` の
                // `const X` と内側 `function X` の両方がこれに当たる。
                let is_decl = matches!(last_ident, "const" | "let" | "var" | "function" | "class");
                if !is_decl && (is_call || is_member || is_typeof || is_new) {
                    return false;
                }
                let is_jsx = before == Some(b'<') || (i >= 2 && &bytes[i - 2..i] == b"</");
                if !is_jsx && !is_decl {
                    // JSX でも宣言でも値利用でもない裸の出現は判定不能 → 安全側 (blocking)
                    return false;
                }
            }
        }
        i += 1;
    }
    saw_occurrence
}

/// TS/TSX/JS の exported object (`export const X = { ... }`) のプロパティ削除を互換変更
/// (`unused_object_members`) として判定する。
///
/// initializer の object literal を flat object または homogeneous record として抽出し、
/// 削除された schema キーが無い (追加のみ) か、削除された schema キーすべてが repo 全体で
/// member access (`.key` / `['key']` / `["key"]`) として参照されていない場合に降格する。
/// 値のみ変更 / spread / computed key / mixed shape / record schema 不揃い / object でない /
/// 抽出不能 / 同名複数宣言 / 削除キーの参照残存はすべて blocking 維持 (false negative 回避)。
#[allow(clippy::too_many_arguments)]
fn detect_object_members_compatible_mod(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    let lang =
        lang_id.filter(|l| matches!(l, LangId::Typescript | LangId::Tsx | LangId::Javascript))?;
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(old_path, "diff file path").ok()?;
    let old_output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{old_path}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !old_output.status.success() {
        return None;
    }
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new_source = parser::read_file(new_utf8).ok()?;
    let old_keys = extract_object_member_keys(&old_output.stdout, lang, name)?;
    let new_keys = extract_object_member_keys(&new_source, lang, name)?;
    if old_keys.record_keys.is_some() != new_keys.record_keys.is_some() {
        return None;
    }
    let has_added_member = new_keys
        .member_keys
        .difference(&old_keys.member_keys)
        .next()
        .is_some();
    let has_added_record_entry = match (&old_keys.record_keys, &new_keys.record_keys) {
        (Some(old_record), Some(new_record)) => {
            // record entry の削除は dynamic access (`config[id]`) を静的保証できないため blocking。
            if old_record.difference(new_record).next().is_some() {
                return None;
            }
            new_record.difference(old_record).next().is_some()
        }
        (None, None) => false,
        _ => return None,
    };
    let removed_members: Vec<&String> = old_keys
        .member_keys
        .difference(&new_keys.member_keys)
        .collect();
    if removed_members.is_empty() && !has_added_member && !has_added_record_entry {
        return None;
    }
    // 削除された schema キー (old にあって new にない)。各キーへの member access が repo
    // 全体で残っていれば破壊的なので blocking 維持。
    for key in removed_members {
        if key_has_member_access_ref(dir, key) {
            return None;
        }
    }
    Some(CompatibleApiModification {
        name: name.to_string(),
        kind: kind.to_string(),
        file: new_path.to_string(),
        old_signature: Some(old_sig.to_string()),
        new_signature: Some(new_sig.to_string()),
        reason: "unused_object_members".to_string(),
    })
}

#[derive(Debug, Clone)]
struct ObjectMemberKeys {
    member_keys: HashSet<String>,
    record_keys: Option<HashSet<String>>,
}

/// TS/TSX/JS ソースから、トップレベル exported な `name` の初期化子 object literal の
/// member schema を抽出する。
///
/// - flat object: top-level key を `member_keys` とする
/// - homogeneous record: top-level key を `record_keys`、各 value object の共通 key を
///   `member_keys` とする
///
/// `as const` / `satisfies T` は unwrap する。object literal でない / spread / computed key /
/// mixed shape / record schema 不揃い / 宣言が見つからない / 同名複数なら None (呼び出し側で
/// blocking 維持)。
fn extract_object_member_keys(
    source: &[u8],
    lang_id: crate::language::LangId,
    name: &str,
) -> Option<ObjectMemberKeys> {
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    let decls = find_toplevel_decls_named(root, name, source);
    if decls.len() != 1 {
        return None;
    }
    let value = decls[0].child_by_field_name("value")?;
    let obj = unwrap_to_object_literal(value)?;
    collect_object_keys(obj, source)
}

/// `expr as const` / `expr satisfies T` をはがして object literal ノードを返す。
fn unwrap_to_object_literal(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "object" => return Some(cur),
            "as_expression" | "satisfies_expression" => {
                cur = cur.named_child(0)?;
            }
            _ => return None,
        }
    }
}

/// object literal の shape を flat / homogeneous record に分類して property キーを集める。
/// mixed shape / record schema 不揃い / spread (`...x`) / computed key (`[expr]:`) があれば None。
fn collect_object_keys(obj: tree_sitter::Node, source: &[u8]) -> Option<ObjectMemberKeys> {
    let mut top_level_keys = HashSet::new();
    let mut record_member_keys: Option<HashSet<String>> = None;
    let mut has_object_value = false;
    let mut has_non_object_value = false;
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                top_level_keys.insert(object_key_text(key, source)?);
                if let Some(value) = child.child_by_field_name("value")
                    && let Some(nested) = unwrap_to_object_literal(value)
                {
                    has_object_value = true;
                    let nested_keys = collect_flat_object_keys(nested, source)?;
                    match &record_member_keys {
                        Some(existing) if existing != &nested_keys => return None,
                        Some(_) => {}
                        None => record_member_keys = Some(nested_keys),
                    }
                } else {
                    has_non_object_value = true;
                }
            }
            "shorthand_property_identifier" => {
                top_level_keys.insert(child.utf8_text(source).ok()?.to_string());
                has_non_object_value = true;
            }
            // spread は shape を静的確定できないので blocking
            "spread_element" => return None,
            _ => {}
        }
    }
    if has_object_value && has_non_object_value {
        return None;
    }
    if has_object_value {
        return Some(ObjectMemberKeys {
            member_keys: record_member_keys?,
            record_keys: Some(top_level_keys),
        });
    }
    Some(ObjectMemberKeys {
        member_keys: top_level_keys,
        record_keys: None,
    })
}

/// flat object として 1 階層分の property キーだけを抽出する。nested object を再帰しない。
fn collect_flat_object_keys(obj: tree_sitter::Node, source: &[u8]) -> Option<HashSet<String>> {
    let mut keys = HashSet::new();
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                keys.insert(object_key_text(key, source)?);
            }
            "shorthand_property_identifier" => {
                keys.insert(child.utf8_text(source).ok()?.to_string());
            }
            "spread_element" => return None,
            _ => {}
        }
    }
    Some(keys)
}

fn object_key_text(key: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match key.kind() {
        "property_identifier" | "shorthand_property_identifier" => {
            Some(key.utf8_text(source).ok()?.to_string())
        }
        "string" => Some(static_js_string_text(key, source)?.to_string()),
        // computed key は静的解析できないので blocking
        _ => None,
    }
}

/// `key` への member access (`.key` / `['key']` / `["key"]`) が repo 全体に残っているか。
/// 解析失敗は保守的に true (残存ありとみなし blocking)。
fn key_has_member_access_ref(dir: &str, key: &str) -> bool {
    if key.is_empty() {
        return true;
    }
    let files = match crate::engine::refs::collect_files(std::path::Path::new(dir), None) {
        Ok(files) => files,
        Err(_) => return true,
    };
    files
        .into_par_iter()
        .any(|path| file_has_member_access_ref(path.as_path(), key).unwrap_or(true))
}

fn file_has_member_access_ref(path: &std::path::Path, key: &str) -> Result<bool> {
    use crate::language::LangId;
    let Some(path_str) = path.to_str() else {
        return Ok(true);
    };
    let utf8_path = camino::Utf8Path::new(path_str);
    let lang = match LangId::from_path(utf8_path) {
        Ok(lang @ (LangId::Javascript | LangId::Typescript | LangId::Tsx)) => lang,
        Err(_) if path.extension().is_none() => {
            let source = parser::read_file(utf8_path)?;
            return match LangId::detect(utf8_path, source.as_bytes()) {
                Ok(lang @ (LangId::Javascript | LangId::Typescript | LangId::Tsx)) => {
                    source_has_member_access_ref(source.as_bytes(), lang, key)
                }
                Ok(_) | Err(_) => Ok(false),
            };
        }
        Ok(_) | Err(_) => return Ok(false),
    };
    let source = parser::read_file(utf8_path)?;
    source_has_member_access_ref(source.as_bytes(), lang, key)
}

fn source_has_member_access_ref(
    source: &[u8],
    lang: crate::language::LangId,
    key: &str,
) -> Result<bool> {
    if memchr::memmem::find(source, key.as_bytes()).is_none() {
        return Ok(false);
    }
    let tree = parser::parse_source(source, lang)?;
    Ok(ast_has_member_access_ref(tree.root_node(), source, key))
}

fn ast_has_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    if node_is_member_access_ref(node, source, key) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| ast_has_member_access_ref(child, source, key))
}

fn node_is_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    match node.kind() {
        "member_expression" => {
            node.child_by_field_name("property")
                .and_then(|property| property.utf8_text(source).ok())
                == Some(key)
        }
        "subscript_expression" => {
            node.child_by_field_name("index")
                .filter(|index| index.kind() == "string")
                .and_then(|index| static_js_string_text(index, source))
                == Some(key)
        }
        _ => false,
    }
}

fn static_js_string_text<'a>(node: tree_sitter::Node, source: &'a [u8]) -> Option<&'a str> {
    let raw = node.utf8_text(source).ok()?;
    let bytes = raw.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    let end = *bytes.last()?;
    if matches!(quote, b'\'' | b'"' | b'`') && quote == end {
        Some(&raw[1..raw.len() - 1])
    } else {
        None
    }
}

/// modified シンボルの全 cross-file 参照が同一 diff 内の変更 hunk で追随済みかを判定する。
///
/// 全ての非定義参照が diff_files の変更 hunk (new 範囲) に収まれば、呼び出し側が同一
/// コミットで更新済みとみなし closed-in-diff (informational)。同名定義が複数 (別型の同名
/// メソッド等) / refs 解析失敗 / diff 外 or hunk 外の参照が 1 つでもあれば false を返し、
/// 保守的に blocking 側 (通常の api.mod) へ倒す。
fn is_modified_closed_in_diff(
    dir: &str,
    name: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> bool {
    use crate::models::reference::RefKind;
    use std::collections::{HashMap, HashSet};
    let bare = bare_name(name);
    let refs = match crate::engine::refs::find_references(bare, std::path::Path::new(dir), None) {
        Ok(r) => r,
        Err(_) => return false,
    };
    // 同名定義が 1 つでなければ曖昧 (別型の同名メソッド等) なので保守的に blocking。
    let def_count = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .count();
    if def_count != 1 {
        return false;
    }
    // import / use 宣言の参照は signature 変更に追随する必要がない (名前だけの import で、
    // 通常は変更されず context 行に残る) ため除外し、実際の呼び出し参照のみを対象とする。
    // 呼び出し参照が 1 件も無ければ (import のみ / 呼び出しが diff 外にある可能性) closed
    // 扱いにせず blocking。
    // 行頭テキスト判定 (ref_is_import_line) は複数行 grouped use ブロックの継続行
    // (`    a, b, cmd_cochange, ...` のように `use ` で始まらない行) を拾えないため、
    // AST ベースの import 行集合でも除外する。import/use 文内の参照は signature 変更に
    // 追随する必要がない (api.mod 誤検出 2026-05-31: grouped use 継続行を未更新 caller と
    // 誤判定して blocking していた問題への対応)。
    let mut import_lines_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    let call_refs: Vec<&crate::models::reference::SymbolReference> = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition) && !ref_is_import_line(r))
        .filter(|r| {
            let import_lines = import_lines_cache
                .entry(r.path.clone())
                .or_insert_with(|| import_statement_lines_for_ref(dir, &r.path));
            !import_lines.contains(&r.line)
        })
        .collect();
    if call_refs.is_empty() {
        return false;
    }
    // 全ての呼び出し参照が、diff 内ファイルかつ実際の追加/変更行 (context 行ではない) にあるか。
    // HunkInfo の new 範囲は context 行を含むため、git diff から実 `+` 行集合を取得して照合する
    // (codex 指摘: context 行に古い呼び出しが入ると未更新 caller を誤って closed 判定してしまう)。
    let mut changed_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    for r in &call_refs {
        let Some(df) = diff_files.iter().find(|df| {
            df.new_path != "/dev/null" && diff_path_matches_ref(&df.new_path, &r.path, dir)
        }) else {
            return false; // diff 外ファイルの参照 → 未更新 caller の可能性
        };
        let changed = changed_cache
            .entry(df.new_path.clone())
            .or_insert_with(|| changed_new_lines_for_file(dir, base, &df.old_path, &df.new_path));
        if !changed.contains(&r.line) {
            return false; // context 行 (未変更) の参照 → 未更新 caller の可能性
        }
    }
    true
}

/// 参照行が import / use 宣言 (signature 変更に追随不要) かを行テキストで簡易判定する。
fn ref_is_import_line(r: &crate::models::reference::SymbolReference) -> bool {
    r.context
        .as_deref()
        .map(|c| {
            let t = c.trim_start();
            t.starts_with("use ")
                || t.starts_with("pub use ")
                || t.starts_with("import ")
                || t.starts_with("from ")
        })
        .unwrap_or(false)
}

/// 参照ファイルの import/use 文が占める行集合 (0-indexed、`SymbolReference.line` と同基準)
/// を tree-sitter AST から取得する。複数行 grouped use ブロックの継続行も含む。
/// parse 不能 (lexer-only 言語 / 拡張子未対応 / 読み込み失敗) の場合は空集合を返し、
/// 既存の除外挙動を変えない。
fn import_statement_lines_for_ref(dir: &str, ref_path: &str) -> std::collections::HashSet<usize> {
    use std::collections::HashSet;
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return HashSet::new();
    };
    let Ok(lang) = crate::language::LangId::from_path(utf8) else {
        return HashSet::new();
    };
    // lexer-only 言語 (Xojo) は ts_language() が panic するため parse しない。
    if lang.is_lexer_only() {
        return HashSet::new();
    }
    let Ok(source) = parser::read_file(utf8) else {
        return HashSet::new();
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return HashSet::new();
    };
    crate::engine::imports::import_statement_lines(tree.root_node())
}

/// `git diff <base> -M -- <old_path> <new_path>` を解析し、new 側で実際に追加/変更された
/// 0-indexed 行集合を返す。取得・解析に失敗した場合は空集合 (= どの参照も追随済みと見なさず
/// blocking 維持) を返す。
fn changed_new_lines_for_file(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
) -> std::collections::HashSet<usize> {
    use std::collections::HashSet;
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(new_path, "diff file path").is_err()
    {
        return HashSet::new();
    }
    // rename 検出を有効化 (-M)。rename された caller を new_path だけの pathspec で diff すると
    // Git は「新規ファイル全行追加」として返し、未更新の古い呼び出しまで changed に見えてしまう
    // (codex 指摘)。old_path も pathspec に含めて rename-aware な diff にする。
    let mut args: Vec<&str> = vec!["diff", base, "-M", "--"];
    if old_path != "/dev/null" && old_path != new_path {
        if validate_git_revision(old_path, "diff file path").is_err() {
            return HashSet::new();
        }
        args.push(old_path);
    }
    args.push(new_path);
    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let diff = String::from_utf8_lossy(&output.stdout);
    crate::engine::diff::extract_changed_new_lines(&diff, new_path)
}

/// diff の new_path (dir 相対) と参照 path (dir 相対 or 絶対) が同一ファイルを指すか判定する。
fn diff_path_matches_ref(diff_path: &str, ref_path: &str, dir: &str) -> bool {
    if diff_path == ref_path {
        return true;
    }
    let abs_diff = std::path::Path::new(dir).join(diff_path);
    let abs_ref = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    match (
        std::fs::canonicalize(&abs_diff),
        std::fs::canonicalize(&abs_ref),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// src 相対パスを Rust モジュールセグメント列に変換する。
/// `meeting/macos.rs` → `[meeting, macos]`、`meeting/mod.rs` → `[meeting]`、
/// `lib.rs` / `main.rs` → `[]` (root モジュール)。
fn module_path_segments(rel: &std::path::Path) -> Vec<String> {
    let comps: Vec<_> = rel.components().collect();
    let mut segs: Vec<String> = Vec::new();
    let last = comps.len().saturating_sub(1);
    for (i, c) in comps.iter().enumerate() {
        let name = c.as_os_str().to_string_lossy();
        if i == last {
            let stem = std::path::Path::new(name.as_ref())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string());
            match stem.as_deref() {
                // mod.rs / lib.rs / main.rs はそのディレクトリのモジュール自身を表す
                Some("mod") | Some("lib") | Some("main") => {}
                Some(s) => segs.push(s.to_string()),
                None => {}
            }
        } else {
            segs.push(name.to_string());
        }
    }
    segs
}

/// 親モジュールファイル直下の `mod <mod_name>` 宣言の可視性 (制限なし pub か) を返す。
///
/// source_file 直下の `mod_item` のみを見る。inline mod (`mod foo { mod bar; }`) 内の同名
/// 宣言は別モジュールスコープの宣言なので拾わない (codex 指摘: 再帰探索で別スコープの同名
/// mod を誤って拾うと可視性判定が壊れる)。
fn find_mod_decl_visibility(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    mod_name: &str,
) -> Option<bool> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "mod_item"
            && child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(mod_name)
        {
            // #[path = "..."] でファイル名と module 名がずれる場合、モジュール解決を諦めて
            // 「判定不能」(None) を返す。下流 (rust_private_module_info_at_base /
            // public_reachable_modules_at_base) は api.rm 抑制を諦め、削除を残す方向に倒す。
            if rust_mod_item_has_path_attribute(child, source) {
                return None;
            }
            let mut mc = child.walk();
            let is_pub = child.children(&mut mc).any(|c| {
                c.kind() == "visibility_modifier" && c.utf8_text(source).map(str::trim) == Ok("pub")
            });
            return Some(is_pub);
        }
    }
    None
}

/// 同名・同種別・同シグネチャの api.add / api.rm ペアを `moved` として相殺する。
///
/// `all_new_candidates` は `added` フィルタ適用前の新規側候補一覧（`added` の上位集合）。
/// `is_used_in_diff_paths` などで `added` から落ちた候補も `removed` との突き合わせに
/// 利用するため、別系統で渡す。
///
/// 戻り値:
/// - `kept_added`: `moved` で相殺されなかった追加シンボル
/// - `kept_removed`: `moved` で相殺されなかった削除シンボル
/// - `moved`: `from`/`to` のペアにまとめた移動シンボル
fn reconcile_with_moves(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    all_new_candidates: Vec<ApiSymbolCandidate>,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedSymbol>,
) {
    use std::collections::HashMap;
    use std::collections::VecDeque;

    // 1) removed を (name, kind, signature) でバケット化。
    let mut removed_bucket: HashMap<(String, String, String), VecDeque<ApiSymbolCandidate>> =
        HashMap::new();
    for sym in removed {
        removed_bucket
            .entry((sym.name.clone(), sym.kind.clone(), sym.signature.clone()))
            .or_default()
            .push_back(sym);
    }

    // 2) 新規候補を順に走査して removed と突き合わせ、`moved` を組み立てる。
    //    同じ (name, kind, signature, file) の重複候補は最初の 1 件だけ扱う。
    //    (name, kind, signature) を共有する複数 add が同じ removed と組まないように、
    //    一度マッチした new 側は `matched_new_files` に記録しておき、後で `added` から
    //    除外する。
    let mut moved: Vec<MovedSymbol> = Vec::new();
    let mut seen_new_keys: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    let mut matched_new_files: HashMap<
        (String, String, String),
        std::collections::HashSet<String>,
    > = HashMap::new();
    for new in &all_new_candidates {
        let dedup_key = (
            new.name.clone(),
            new.kind.clone(),
            new.signature.clone(),
            new.file.clone(),
        );
        if !seen_new_keys.insert(dedup_key) {
            continue;
        }
        let bucket_key = (new.name.clone(), new.kind.clone(), new.signature.clone());
        if let Some(bucket) = removed_bucket.get_mut(&bucket_key)
            && let Some(rm) = bucket.pop_front()
        {
            matched_new_files
                .entry(bucket_key)
                .or_default()
                .insert(new.file.clone());
            moved.push(MovedSymbol {
                name: rm.name,
                kind: rm.kind,
                from: rm.file,
                to: new.file.clone(),
            });
        }
    }

    // 3) `moved` で相殺された候補は `added` からも除外する。
    let kept_added: Vec<ApiSymbolCandidate> = added
        .into_iter()
        .filter(|a| {
            let key = (a.name.clone(), a.kind.clone(), a.signature.clone());
            !matched_new_files
                .get(&key)
                .map(|files| files.contains(&a.file))
                .unwrap_or(false)
        })
        .collect();

    // 4) ペア化されなかった `removed` を集める。
    let kept_removed: Vec<ApiSymbolCandidate> = removed_bucket
        .into_values()
        .flat_map(|bucket| bucket.into_iter())
        .collect();

    (kept_added, kept_removed, moved)
}

/// qualname (`Container.method`) から末尾セグメントのみを抜き出す。
/// `a.b.c` → `c`、`foo` → `foo`。
pub(crate) fn bare_name(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// ファイルリストからエクスポートシンボルを収集し、参照ゼロのシンボルを返す。
/// dead-code コマンドと review コマンドの共通コアロジック。
/// count_non_definition_refs_split で production / test 別に件数のみカウントし、
/// SymbolReference を確保しない。
/// `files` は呼び出し側で `--glob` 等のフィルタを適用済み。
/// refs 探索は `--dir` 全体で実施する (F3 修正: `--glob` で refs スコープが
/// 狭まると、フィルタ外のファイルから同シンボルを参照している場合に dead
/// 判定が誤陽性になるため)。
///
/// 戻り値は `(dead_symbols, test_only_symbols)`:
/// - `dead_symbols`: production / test どちらからも参照されないシンボル
/// - `test_only_symbols`: test/spec 配下からのみ参照されるシンボル (F5)
mod dead_code;

pub use dead_code::cmd_dead_code;
#[cfg(test)]
pub(crate) use dead_code::{auto_detect_framework, extract_dead_code_candidates_from_file};
pub(crate) use dead_code::{
    collect_python_unittest_classes, detect_dead_symbols_from_files, enclosing_container,
    filter_dead_by_touched_symbols, filter_diff_files_for_dead_code, is_phpunit_test_symbol,
    is_python_test_symbol, is_test_path, resolve_dead_code_excludes,
    resolve_framework_globs_with_auto_detect,
};

fn extract_exported_symbols_from_git(
    dir: &str,
    base: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // テストファイル配下のシンボルは API 差分検出の対象外。
    // (api.rm の base 側比較もテストファイルからは行わない)
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    // `base` と `file_path` はオプション誤認識を避けるため先頭が `-` のものを拒否する
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(file_path, "diff file path").ok()?;
    let output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    extract_exported_symbols_from_source(file_path, &output.stdout)
}

/// 与えられた旧側ソースから export シンボル一覧を抽出する。
///
/// `extract_exported_symbols_from_git` のフォールバックとして、`--diff-file` の削除 hunk から
/// 復元した旧ソースを直接渡す経路で使う。test path 判定とフィルタは git 経路と同一。
fn extract_exported_symbols_from_source(
    file_path: &str,
    source: &[u8],
) -> Option<Vec<(String, String, String)>> {
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    let utf8_path = camino::Utf8Path::new(file_path);
    let lang_id = crate::language::LangId::from_path(utf8_path).ok()?;
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;
    // Rust の `impl Trait for Type` 配下のメソッドは trait の実装事実であり、独立した
    // 公開 API item ではない。module 移動など実体は維持したままの変更でも api.add / api.rm
    // に誤計上されるのを避けるため、API 変更検出でも trait impl メソッドを除外する。
    // 旧側を読む経路は API 変更検出 (api.rm 比較) のみで使われる。
    // dead-code は最新コミット側だけを見るため framework entrypoint の除外は不要。
    Some(filter_exported_symbols(
        &syms,
        root,
        source,
        lang_id,
        true,
        false,
        Some(file_path),
    ))
}

fn extract_exported_symbols_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // テストファイル配下のシンボルは外部 API 面ではないため、api.add/rm/mod の
    // 検出対象から外す。Swift Testing (`@Test`/`@Suite`)、JUnit テストメソッド、
    // *.test.ts、tests/ ディレクトリ等が該当する。
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    // API 変更検出ではフレームワーク登録デコレータ付き関数も「公開 API 面」として
    // 検出対象に残す (新規 CLI サブコマンドの追加・削除も api.add / api.rm として
    // 報告したい)。
    extract_exported_symbols_from_file_inner(dir, file_path, true, false)
}

pub(crate) fn extract_exported_symbols_from_file_inner(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
) -> Option<Vec<(String, String, String)>> {
    // diff から得た file_path は信頼境界外。`../etc/passwd` 等のトラバーサルや絶対パスを
    // 拒否し、workspace 外のファイルを誤って読まないようにする。
    if !crate::engine::impact::is_safe_diff_path(file_path) {
        return None;
    }
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = camino::Utf8Path::new(full_path.to_str()?);
    let source = parser::read_file(utf8_path).ok()?;
    let lang_id = crate::language::LangId::from_path(utf8_path).ok()?;

    // lexer-only 言語 (現状 Xojo) は tree-sitter を持たないため、lexer 経由で
    // export 相当のシンボルを抽出する。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        return Some(crate::engine::lexer::extract_exported_symbols(
            &source,
            lexer_lang,
            exclude_framework_entrypoints,
        ));
    }

    let tree = parser::parse_source(&source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, &source, lang_id).ok()?;
    Some(filter_exported_symbols(
        &syms,
        root,
        &source,
        lang_id,
        exclude_trait_impls,
        exclude_framework_entrypoints,
        Some(file_path),
    ))
}

/// 新ツリーのファイルから TS/JS の named re-export 名集合を取得する。api.rm 抑制に使う。
/// 非 TS/JS、parse 失敗、安全でないパスでは空集合を返す (fail-safe: 抑制しない方向)。
fn extract_reexported_names_from_file(
    dir: &str,
    file_path: &str,
) -> std::collections::HashSet<String> {
    if !crate::engine::impact::is_safe_diff_path(file_path) {
        return std::collections::HashSet::new();
    }
    let full_path = std::path::Path::new(dir).join(file_path);
    let Some(utf8) = full_path.to_str() else {
        return std::collections::HashSet::new();
    };
    let utf8_path = camino::Utf8Path::new(utf8);
    let Ok(lang_id) = crate::language::LangId::from_path(utf8_path) else {
        return std::collections::HashSet::new();
    };
    // 現状 re-export 認識を実装している言語のみ対象 (TS/JS の `export { x } from "..."`,
    // Rust の `pub use sub::x;`)。他言語は将来対応。
    if !matches!(
        lang_id,
        crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
            | crate::language::LangId::Rust
    ) {
        return std::collections::HashSet::new();
    }
    let Ok(source) = parser::read_file(utf8_path) else {
        return std::collections::HashSet::new();
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return std::collections::HashSet::new();
    };
    match lang_id {
        crate::language::LangId::Rust => {
            crate::engine::symbols::collect_rust_reexported_names(tree.root_node(), &source)
        }
        _ => crate::engine::symbols::collect_reexported_names(tree.root_node(), &source),
    }
}

/// dead_symbols のうち、宣言行が今回の diff の追加行 (`+` 行) と重なるもののみを残す。
///
/// `--dead-scope touched-symbols` の実装。`review --hook` のデフォルトとして使われ、
/// 「changed file 内に元からあった dead」がレビューノイズとして毎回出る UX 問題を
/// 解消する。
///
/// 注意: `HunkInfo` の `new_start` / `new_count` は context 行も含むため
/// hunk 範囲全体を「touched」と扱うと既存 dead まで残してしまう。ここでは
/// `extract_changed_new_lines` で **実際に追加された行** だけを set 化して照合する。
pub(crate) fn extract_symbol_lines(
    dir: &str,
    file_path: &str,
) -> Option<std::collections::HashMap<String, usize>> {
    use std::collections::HashMap;
    let full = std::path::Path::new(dir).join(file_path);
    let utf8 = camino::Utf8Path::new(full.to_str()?);
    let source = parser::read_file(utf8).ok()?;
    let lang_id = crate::language::LangId::from_path(utf8).ok()?;

    let symbols = if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        crate::engine::lexer::extract_symbols(&source, lexer_lang)
    } else {
        let tree = parser::parse_source(&source, lang_id).ok()?;
        crate::engine::symbols::extract_symbols(tree.root_node(), &source, lang_id).ok()?
    };

    let mut map = HashMap::new();
    for s in symbols {
        // 同名シンボルが複数ある場合、最初に出現した行を保持する。
        map.entry(s.name).or_insert(s.range.start.line);
    }
    Some(map)
}

/// シンボルの種類に応じた API シグネチャを抽出する。
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 宣言行のみ。
///
/// クラス/型は宣言行（`class Foo(Bar):` や `struct Foo {` など）のみをシグネチャとする。
/// 本体（メソッド本体や private フィールド）の変更でクラス全体の API 変更として
/// 再検出されるのを避けるため、メンバーの集約はしない。
/// メンバー個々の変更は method シンボル単独で検出される。
///
/// function / method の場合は tree-sitter ノードで「宣言開始から body 直前まで」を
/// 抽出し、whitespace を正規化して signature とする。これにより `where` 句や複数行
/// generics で先頭行が同一でも引数列が変わったケース (Issue
/// 2026-05-14-rename-and-multiline-signature) を検出できる。
/// body が無い (interface method / abstract 等) や node 取得失敗時は先頭行を fallback。
fn extract_api_signature(
    sym: &crate::models::symbol::Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    lang_id: crate::language::LangId,
) -> String {
    use crate::models::symbol::SymbolKind;
    if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
        let start = tree_sitter::Point {
            row: sym.range.start.line,
            column: sym.range.start.column,
        };
        let end = tree_sitter::Point {
            row: sym.range.end.line,
            column: sym.range.end.column,
        };
        if let Some(node) = root.descendant_for_point_range(start, end) {
            let mut cur = node;
            loop {
                match cur.kind() {
                    "function_item"
                    | "function_declaration"
                    | "function_definition"
                    | "method_declaration"
                    | "method_definition"
                    | "function_signature_item"
                    // Swift protocol requirement (body なしの宣言)。複数行 requirement でも
                    // 先頭行 fallback でなく AST から signature 全体を抽出する (codex 指摘)。
                    | "protocol_function_declaration" => {
                        let s = cur.start_byte();
                        let e = cur
                            .child_by_field_name("body")
                            .map(|b| b.start_byte())
                            .unwrap_or_else(|| cur.end_byte());
                        // TS/TSX の関数 destructured params (`function foo({ a, b }: T)`) は
                        // `{ ... }` 内の variable 列が変わっても呼び出し側契約 (`: T` 型注釈)
                        // に影響しないため、signature 比較から除外する。React の Props
                        // 拡張 (optional prop 追加 + destructure 受け取り追加) で api.mod に
                        // 出る false positive を防ぐ (Issue
                        // 2026-05-28-api-mod-optional-props-additive 対応)。
                        if matches!(
                            lang_id,
                            crate::language::LangId::Typescript | crate::language::LangId::Tsx
                        ) {
                            return normalize_typescript_destructure_signature(cur, source, s, e);
                        }
                        // Tauri command (`#[tauri::command]` / `#[command]`) の自動注入型引数
                        // (AppHandle / State / Window 等) は実行時に Tauri が注入し JS 側 invoke()
                        // の引数には現れないため、signature 比較から除外する
                        // (Issue 2026-05-29-swift-sidecar-api-mod パターンB)。
                        if lang_id == crate::language::LangId::Rust
                            && let Some(sig) =
                                normalize_rust_tauri_command_signature(cur, source, s, e)
                        {
                            return sig;
                        }
                        if let Some(bytes) = source.get(s..e) {
                            return normalize_signature_whitespace(bytes);
                        }
                        break;
                    }
                    _ => {}
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }

    // フォールバック: 先頭行のみ
    lines
        .get(sym.range.start.line)
        .unwrap_or(&"")
        .trim()
        .to_string()
}

/// 値バインディング (const / static / export const) の宣言から抽出した shape 情報。
/// initializer (= 右辺) を除いた宣言の骨格と、value-only 変更を安全に判定するための補助情報。
struct BindingShape {
    /// initializer を除いた正規化済み宣言テキスト (名前・型・visibility・binding kind を含む)。
    shape: String,
    /// 不変バインディング (Rust `const` / 非 mut `static`、TS/JS `const`) なら true。
    /// mutable (`static mut` / `let` / `var`) は false。
    is_const_binding: bool,
    /// 型注釈を持つなら true (TS の型注釈なし initializer の安全判定に使う)。
    has_type_annotation: bool,
    /// initializer が scalar literal (数値 / 文字列 / 真偽値 / null 等) なら true。
    /// 関数 / object / array / call 等の複雑な式は false。
    initializer_is_scalar: bool,
}

/// `node` を起点に、指定 kind のいずれかに最初に一致する子孫ノードを深さ優先で探す。
/// signature 文字列は単一宣言なので export_statement 等のラップを潜るために使う。
fn find_first_descendant_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.contains(&node.kind()) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_first_descendant_of_kinds(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// value 手前で切った宣言テキストを正規化する。末尾に残る `=` と前後・連続空白を畳む。
fn normalize_binding_shape_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_end();
    // value の直前で切ると末尾に `= ` が残るため取り除く。
    let without_eq = trimmed
        .strip_suffix('=')
        .map(str::trim_end)
        .unwrap_or(trimmed);
    without_eq.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// signature 文字列を AST パースし、値バインディングなら initializer を除いた shape を返す。
/// 対象外 (関数 / 型 / バインディング以外) や抽出失敗時は None を返し、呼び出し側は保守的に
/// 従来どおり api.mod へ倒す (codex 設計合意: テキストの `=` 分割ではなく AST ベース)。
fn extract_binding_shape(sig: &str, lang_id: crate::language::LangId) -> Option<BindingShape> {
    // lexer-only 言語は tree-sitter を持たないため対象外。
    if lang_id.is_lexer_only() {
        return None;
    }
    let source = sig.as_bytes();
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    match lang_id {
        crate::language::LangId::Rust => {
            let decl = find_first_descendant_of_kinds(root, &["const_item", "static_item"])?;
            extract_rust_binding_shape(decl, source)
        }
        crate::language::LangId::Typescript
        | crate::language::LangId::Tsx
        | crate::language::LangId::Javascript => {
            let decl = find_first_descendant_of_kinds(
                root,
                &["lexical_declaration", "variable_declaration"],
            )?;
            extract_js_binding_shape(decl, source)
        }
        _ => None,
    }
}

/// Rust の const_item / static_item から shape を抽出する。
fn extract_rust_binding_shape(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<BindingShape> {
    // static mut は mutable_specifier を子に持つ。const は常に不変。
    let mut cursor = node.walk();
    let is_mut = node
        .children(&mut cursor)
        .any(|c| c.kind() == "mutable_specifier");
    let value = node.child_by_field_name("value");
    let has_type_annotation = node.child_by_field_name("type").is_some();
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let shape_bytes = source.get(node.start_byte()..shape_end)?;
    let initializer_is_scalar = value.map(rust_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding: !is_mut,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// TS/JS の lexical_declaration / variable_declaration から shape を抽出する。
fn extract_js_binding_shape(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<BindingShape> {
    // binding kind (`const` / `let` / `var`) を最初の anonymous child から判定する。
    let mut decl_cursor = node.walk();
    let binding_kw = node
        .children(&mut decl_cursor)
        .find(|c| matches!(c.kind(), "const" | "let" | "var"))
        .map(|c| c.kind());
    let is_const_binding = binding_kw == Some("const");

    // 複数 declarator (`const a = 1, b = 2;`) は shape 抽出が壊れるため対象外。
    let mut declarators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            declarators.push(child);
        }
    }
    if declarators.len() != 1 {
        return None;
    }
    let declarator = declarators[0];
    let value = declarator.child_by_field_name("value");
    let has_type_annotation = declarator.child_by_field_name("type").is_some();

    // visibility (export) を shape に含めるため、親が export_statement なら起点を遡る。
    let shape_start = match node.parent() {
        Some(p) if p.kind() == "export_statement" => p.start_byte(),
        _ => node.start_byte(),
    };
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| declarator.end_byte());
    let shape_bytes = source.get(shape_start..shape_end)?;
    let initializer_is_scalar = value.map(js_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// Rust の値ノードが scalar literal かを判定する (型注釈なし経路の安全弁、誤検出側に倒す)。
fn rust_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "integer_literal"
            | "float_literal"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "boolean_literal"
    )
}

/// JS/TS の値ノードが scalar literal かを判定する。関数 / object / array / call は false。
fn js_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "number" | "string" | "true" | "false" | "null" | "undefined"
    )
}

/// old/new signature が「const / 非 mut static / export const の値のみ変更 (shape 不変)」かを
/// 判定する。true なら api.mod ではなく const_value_changes (informational) に振り分ける。
///
/// gate: (1) kind が value binding (constant/variable)、(2) 言語が Rust/TS/TSX/JS、
/// (3) 両者が不変バインディング、(4) shape 一致、(5) TS で型注釈なしなら両者 scalar literal。
/// いずれか外れる / 抽出失敗時は false を返し、保守的に api.mod へ倒す。
fn is_const_value_only_change(
    old_sig: &str,
    new_sig: &str,
    kind: &str,
    lang_id: crate::language::LangId,
) -> bool {
    // 値バインディングの kind のみ (Rust const/static="constant"、TS/JS const="variable")。
    if !matches!(kind, "constant" | "variable") {
        return false;
    }
    if !matches!(
        lang_id,
        crate::language::LangId::Rust
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        return false;
    }
    let (Some(old), Some(new)) = (
        extract_binding_shape(old_sig, lang_id),
        extract_binding_shape(new_sig, lang_id),
    ) else {
        return false;
    };
    // mutable バインディング (static mut / let / var) は demote しない。
    if !old.is_const_binding || !new.is_const_binding {
        return false;
    }
    // shape (名前・型・visibility・binding kind) が変われば破壊的変更の可能性 → api.mod。
    if old.shape != new.shape {
        return false;
    }
    // TS/JS で型注釈がない場合、関数 / object / array / call initializer は shape 推定が
    // 危険なため scalar literal 同士のときだけ demote する (codex 指摘)。
    if matches!(
        lang_id,
        crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        let both_typed = old.has_type_annotation && new.has_type_annotation;
        let both_scalar = old.initializer_is_scalar && new.initializer_is_scalar;
        if !both_typed && !both_scalar {
            return false;
        }
    }
    true
}

/// Tauri command の自動注入型 (実行時に Tauri が注入し JS-facing な invoke() 引数に現れない型)。
/// `Channel<T>` は JS 側から渡す引数なので含めない (signature 差分の対象に残す)。
const TAURI_INJECTED_TYPES: &[&str] = &[
    "AppHandle",
    "Window",
    "Webview",
    "WebviewWindow",
    "State",
    "Request",
    "CommandScope",
    "GlobalScope",
];

/// Rust の型ノードから base 名 (パス・参照・ジェネリクスを剥がした末尾型名) を取り出す。
fn rust_type_base_name(ty: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match ty.kind() {
        "type_identifier" => ty.utf8_text(source).ok().map(str::to_string),
        // tauri::AppHandle → name 子 'AppHandle'
        "scoped_type_identifier" => ty
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        // State<'_, T> → base 'State'
        "generic_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        // &State<...> / &AppHandle → 内側の型
        "reference_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        _ => None,
    }
}

/// function_item が Tauri command 属性 (`#[tauri::command]` / `#[command]`) を持つか判定する。
/// Rust では属性は function_item の前方兄弟 (attribute_item) に並ぶ。
fn rust_fn_has_tauri_command_attr(fn_node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut sib = fn_node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                if let Ok(text) = s.utf8_text(source) {
                    let inner = text
                        .trim_start_matches("#[")
                        .trim_start_matches("#![")
                        .trim_end_matches(']')
                        .trim();
                    if inner == "tauri::command"
                        || inner.starts_with("tauri::command(")
                        || inner == "command"
                        || inner.starts_with("command(")
                    {
                        return true;
                    }
                }
            }
            // 属性とコメントは読み飛ばし、それ以外に到達したら属性列の終端
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    false
}

/// Tauri command 関数の signature から自動注入型引数を除外して返す。
/// Tauri command でなければ None を返し、呼び出し側で通常の signature 抽出にフォールバックする。
fn normalize_rust_tauri_command_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    s: usize,
    e: usize,
) -> Option<String> {
    if !rust_fn_has_tauri_command_attr(fn_node, source) {
        return None;
    }
    let params = fn_node.child_by_field_name("parameters")?;
    let mut kept: Vec<String> = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter" => {
                let injected = child
                    .child_by_field_name("type")
                    .and_then(|t| rust_type_base_name(t, source))
                    .is_some_and(|n| TAURI_INJECTED_TYPES.contains(&n.as_str()));
                if !injected && let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            "self_parameter" => {
                if let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    let prefix = source.get(s..params.start_byte())?;
    let suffix = source.get(params.end_byte()..e)?;
    let rebuilt = format!(
        "{}({}){}",
        String::from_utf8_lossy(prefix),
        kept.join(", "),
        String::from_utf8_lossy(suffix)
    );
    Some(normalize_signature_whitespace(rebuilt.as_bytes()))
}

/// TS/TSX 関数の signature を抽出し、parameters 直下の `object_pattern`
/// (destructured params) を `{}` に正規化する。
///
/// `function foo({ a, b, c = 0 }: Props)` と `function foo({ a, b }: Props)` は
/// どちらも呼び出し側契約は `: Props` のみで、destructure 中身は内部 binding。
/// 正規化することで Props 拡張に伴う destructure 行の追加が api.mod に出ない。
///
/// 型注釈側の inline object type (`function foo({x}: {x: string, y: number})` の
/// `{x: string, y: number}`) は `type_annotation` 子なので置換対象外。
///
/// 「引数なし `()` から省略可能な destructured 引数追加」の互換性判定は、
/// signature 単独では行わない (型注釈変更だけ起きるケースを誤って互換扱いする
/// リスクがあるため)。両側 signature を見て判定するロジックは
/// [`is_ts_no_arg_to_optional_destructured_compatible`] が detect_api_changes
/// 経路で行う。
fn normalize_typescript_destructure_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    start_byte: usize,
    end_byte: usize,
) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(params) = fn_node.child_by_field_name("parameters") {
        collect_parameter_object_pattern_ranges(params, &mut ranges);
    }
    if ranges.is_empty() {
        if let Some(bytes) = source.get(start_byte..end_byte) {
            return normalize_signature_whitespace(bytes);
        }
        return String::new();
    }
    ranges.sort_by_key(|r| r.0);

    let mut buf: Vec<u8> = Vec::with_capacity(end_byte - start_byte);
    let mut cursor = start_byte;
    for (op_start, op_end) in &ranges {
        if *op_start < cursor || *op_end > end_byte {
            continue;
        }
        if let Some(bytes) = source.get(cursor..*op_start) {
            buf.extend_from_slice(bytes);
        }
        buf.extend_from_slice(b"{}");
        cursor = *op_end;
    }
    if let Some(bytes) = source.get(cursor..end_byte) {
        buf.extend_from_slice(bytes);
    }
    normalize_signature_whitespace(&buf)
}

/// 関数 parameters が「単一の destructured object parameter で、呼び出し側から
/// 引数省略可能 (`foo()` で valid) と判定できる」場合に true。
///
/// 判定基準:
/// - parameters の named child が 1 個 (required_parameter / optional_parameter)
/// - その pattern が object_pattern
/// - 以下のいずれかを満たす:
///   1. parameter に default value (`= {}` 等の initializer) がある
///   2. type annotation の型が「全 optional な object type」と証明できる
///      - inline `object_type` ですべての property が `?` 付き (空も含む)
///      - 同一ファイル内の `interface` / `type alias` で同名のものが見つかり、
///        その body / value が全 optional な object type
///
/// import 型 / generic / intersection / conditional type は False を返す (型推論が
/// 必要なため、AST だけでは省略可能性を保証できない。codex 設計合意)。
fn is_optionally_omittable_single_destructured_param(
    params: tree_sitter::Node<'_>,
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut cursor = params.walk();
    let param_nodes: Vec<tree_sitter::Node<'_>> = params
        .children(&mut cursor)
        .filter(|n| matches!(n.kind(), "required_parameter" | "optional_parameter"))
        .collect();
    if param_nodes.len() != 1 {
        return false;
    }
    let param = param_nodes[0];

    // pattern が object_pattern
    let Some(pattern) = param.child_by_field_name("pattern") else {
        return false;
    };
    if pattern.kind() != "object_pattern" {
        return false;
    }

    // 1. default value (`= {}` 等の initializer) があるなら無条件で省略可能
    if param.child_by_field_name("value").is_some() {
        return true;
    }

    // 2. type annotation を取得 (`: T` の T を取り出す)
    let Some(type_annot) = param.child_by_field_name("type") else {
        return false;
    };
    // type_annotation の named child の最後が型ノード
    let mut tc = type_annot.walk();
    let type_node = type_annot.named_children(&mut tc).last();
    let Some(type_node) = type_node else {
        return false;
    };

    if type_node.kind() == "object_type" {
        return all_object_type_members_optional(type_node, source);
    }
    if type_node.kind() == "type_identifier" {
        let Some(name_bytes) = source.get(type_node.start_byte()..type_node.end_byte()) else {
            return false;
        };
        let Ok(name) = std::str::from_utf8(name_bytes) else {
            return false;
        };
        let decls = collect_top_level_type_decls(root, source, name);
        return !decls.is_empty()
            && decls
                .iter()
                .all(|d| single_type_decl_all_optional(*d, source));
    }
    false
}

/// `object_type` (TS の inline `{ x?: T; y: U }`) のすべての property が `?` 付き
/// optional ならば true。method_signature / index_signature がある場合は false
/// (これらは optional マーカーの一般判定が複雑になるため保守的に拒否)。
/// property が 1 つもない (空 `{}`) ケースも全 optional と同等扱いで true。
fn all_object_type_members_optional(object_type: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = object_type.walk();
    for child in object_type.children(&mut cursor) {
        match child.kind() {
            "property_signature" if !property_signature_has_optional_marker(child, source) => {
                return false;
            }
            "method_signature" | "index_signature" | "construct_signature" | "call_signature" => {
                return false;
            }
            _ => {}
        }
    }
    true
}

/// `property_signature` ノードに optional マーカー `?` が付いているかを tree-sitter
/// の `?` token を直接見て判定する。`"name?": string` のような string property
/// の名前に `?` を含むケースは誤判定しない。
fn property_signature_has_optional_marker(prop: tree_sitter::Node<'_>, _source: &[u8]) -> bool {
    let mut cursor = prop.walk();
    for child in prop.children(&mut cursor) {
        match child.kind() {
            "?" => return true,
            "type_annotation" => return false,
            _ => {}
        }
    }
    false
}

/// `root` のトップレベル (program 直下 / `export_statement` 直下) にある
/// `interface_declaration` / `type_alias_declaration` のうち、name フィールドが
/// 指定名と一致するものを **すべて** 集める。interface declaration merge 対応の
/// ために複数返す。
///
/// ネストした declaration (関数内 / ブロック内) や import 型の解決はしない。
/// 関数 scope などローカル scope の declaration を誤って拾わないため、スコープを
/// トップレベルに限定する (codex 指摘 3 対応)。
fn collect_top_level_type_decls<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Vec<tree_sitter::Node<'a>> {
    let mut decls = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let candidate = if child.kind() == "export_statement" {
            let mut sub_cursor = child.walk();
            child
                .children(&mut sub_cursor)
                .find(|c| matches!(c.kind(), "interface_declaration" | "type_alias_declaration"))
        } else if matches!(
            child.kind(),
            "interface_declaration" | "type_alias_declaration"
        ) {
            Some(child)
        } else {
            None
        };
        if let Some(decl) = candidate
            && let Some(name_node) = decl.child_by_field_name("name")
            && let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte())
            && let Ok(decl_name) = std::str::from_utf8(bytes)
            && decl_name == name
        {
            decls.push(decl);
        }
    }
    decls
}

/// 単一の `interface_declaration` / `type_alias_declaration` のメンバが全 optional な
/// object 型かを判定する。
///
/// - `interface_declaration` が `extends_type_clause` を持つ場合は base interface が
///   required field を持つ可能性があるため保守的に false (codex 指摘 2 対応)
/// - `type_alias_declaration` は value が `object_type` のケースのみ判定対象。
///   union / intersection / generic / conditional / mapped 等は保守的に false
fn single_type_decl_all_optional(decl: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match decl.kind() {
        "interface_declaration" => {
            if interface_has_extends(decl) {
                return false;
            }
            if let Some(body) = decl.child_by_field_name("body") {
                return all_object_type_members_optional(body, source);
            }
            false
        }
        "type_alias_declaration" => {
            if let Some(value) = decl.child_by_field_name("value")
                && value.kind() == "object_type"
            {
                return all_object_type_members_optional(value, source);
            }
            false
        }
        _ => false,
    }
}

/// `interface_declaration` ノードが `extends_type_clause` を持つかを判定する。
fn interface_has_extends(decl: tree_sitter::Node<'_>) -> bool {
    let mut cursor = decl.walk();
    decl.children(&mut cursor)
        .any(|c| c.kind() == "extends_type_clause")
}

/// TS/TSX 関数の「引数なし `()` から省略可能 destructured 引数追加」が
/// backward-compatible かを判定する。両側 signature を見て判定するため
/// `detect_api_changes` から呼ぶ。`extract_api_signature` で signature 単独
/// 正規化に組み込まないのは、optional 型変更 (`{x?:string}` → `{x?:number}`)
/// まで誤って互換扱いするのを防ぐため (codex 設計合意)。
///
/// 条件:
/// 1. `new_path` の言語が TypeScript / Tsx
/// 2. `new_sig` に `fn_name({}` (destructure normalize 済み) が含まれる
///    (早期 reject 用の文字列マッチ)
/// 3. 旧ツリー (`base:old_path`) のトップレベル関数 `fn_name` の parameters が
///    **AST 上で** 空 (codex 指摘: 文字列 contains だと型注釈内 call signature
///    `{ fn_name(): void }` を誤検出するため、必ず AST で確認する)
/// 4. 新ツリー (`new_path`) のトップレベル関数 `fn_name` の parameters が省略
///    可能と判定できる
///
/// `old_path` と `new_path` は rename 差分に対応するため別々に渡す。
fn is_ts_no_arg_to_optional_destructured_compatible(
    old_sig: &str,
    new_sig: &str,
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    fn_name: &str,
) -> bool {
    let full_new_path = std::path::Path::new(dir).join(new_path);
    let Some(utf8_str) = full_new_path.to_str() else {
        return false;
    };
    let utf8_new_path = camino::Utf8Path::new(utf8_str);
    let Ok(lang_id) = crate::language::LangId::from_path(utf8_new_path) else {
        return false;
    };
    if !matches!(
        lang_id,
        crate::language::LangId::Typescript | crate::language::LangId::Tsx
    ) {
        return false;
    }

    // 早期 reject (高速化): 新 sig が destructure 形式でなければ判定不要
    if !signature_has_destructured_params_for(new_sig, fn_name) {
        return false;
    }
    // 早期 reject (高速化): 旧 sig 文字列に `fn_name()` パターンがなければ判定不要。
    // 文字列 contains は false-positive あり (型注釈内 call signature) のため、これは
    // 単なる早期スクリーニング。確実な判定は次の AST 検査で行う。
    if !signature_has_empty_parens_for(old_sig, fn_name) {
        return false;
    }
    // 旧ツリーで AST 検査: トップレベル関数 fn_name の parameters が実際に空か。
    // rename 差分では `df.old_path` を使うため、`old_path` を渡す。
    if !old_top_level_function_has_empty_parameters(dir, base, old_path, lang_id, fn_name) {
        return false;
    }

    let Ok(source) = parser::read_file(utf8_new_path) else {
        return false;
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return false;
    };
    let root = tree.root_node();

    let Some(fn_node) = find_top_level_function_by_name(root, &source, fn_name) else {
        return false;
    };
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return false;
    };
    is_optionally_omittable_single_destructured_param(params, root, &source)
}

/// signature 文字列に `fn_name()` (parameters なし) パターンが含まれるかを判定。
/// 注: これは早期 reject 用のスクリーニング。型注釈内の call signature を誤検出する
/// 可能性があるため、確実な判定には AST 検査 (`old_top_level_function_has_empty_parameters`)
/// を併用する。
fn signature_has_empty_parens_for(sig: &str, fn_name: &str) -> bool {
    let needle = format!("{fn_name}()");
    sig.contains(&needle)
}

/// signature 文字列に destructure normalize 済みの `fn_name({}` パターンが
/// 含まれるかを判定。
fn signature_has_destructured_params_for(sig: &str, fn_name: &str) -> bool {
    let needle = format!("{fn_name}({{}}");
    sig.contains(&needle)
}

/// 旧ツリー (base リビジョン) を `git show` で取得して parse し、トップレベル関数
/// `fn_name` の parameters が空かを AST で判定する。
///
/// signature 文字列の `fn_name()` パターン検査だけでは型注釈内 call signature を
/// 誤検出するため、最終確認として AST 検査が必要。
///
/// `base` / `file_path` は `validate_git_revision` で検証する (codex 指摘: 既存の
/// `extract_exported_symbols_from_git` と同じ防御を行わないと `--diff` / stdin 経路で
/// 未検証の `base` がここに到達し得る)。
fn old_top_level_function_has_empty_parameters(
    dir: &str,
    base: &str,
    file_path: &str,
    lang_id: crate::language::LangId,
    fn_name: &str,
) -> bool {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file_path, "diff file path").is_err()
    {
        return false;
    }
    let output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let source = output.stdout;
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return false;
    };
    let Some(fn_node) = find_top_level_function_by_name(tree.root_node(), &source, fn_name) else {
        return false;
    };
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    params.named_children(&mut cursor).count() == 0
}

/// `root` のトップレベル (program 直下 / `export_statement` 直下) にある関数 /
/// メソッド宣言のうち、name が一致するものを返す。ネストしたローカル関数や
/// 関数式内の同名宣言は対象外 (codex 指摘 6 対応)。
fn find_top_level_function_by_name<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let fn_kinds = |k: &str| {
        matches!(
            k,
            "function_declaration"
                | "function_definition"
                | "method_definition"
                | "function_signature_item"
        )
    };
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let candidate = if child.kind() == "export_statement" {
            let mut sub_cursor = child.walk();
            child.children(&mut sub_cursor).find(|c| fn_kinds(c.kind()))
        } else if fn_kinds(child.kind()) {
            Some(child)
        } else {
            None
        };
        if let Some(fn_node) = candidate
            && let Some(name_node) = fn_node.child_by_field_name("name")
            && let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte())
            && let Ok(decl_name) = std::str::from_utf8(bytes)
            && decl_name == name
        {
            return Some(fn_node);
        }
    }
    None
}

/// TS/TSX の formal_parameters 直下にある `object_pattern` のバイト範囲を集める。
///
/// パラメータの `type_annotation` (inline object type など) には踏み込まないため、
/// 型注釈側の object type は影響を受けない。required_parameter / optional_parameter の
/// `pattern` フィールドを直接見て object_pattern かを判定する。
fn collect_parameter_object_pattern_ranges(
    params: tree_sitter::Node<'_>,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                if let Some(pattern) = child.child_by_field_name("pattern")
                    && pattern.kind() == "object_pattern"
                {
                    ranges.push((pattern.start_byte(), pattern.end_byte()));
                }
            }
            // 無型 JS スタイル: parameter ノードがなく object_pattern が直接子に来る
            // ケース。安全側に倒して同様に正規化する (TS/TSX に限定済み)。
            "object_pattern" => {
                ranges.push((child.start_byte(), child.end_byte()));
            }
            _ => {}
        }
    }
}

/// signature bytes を whitespace で分割して 1 つの space で結合し正規化する。
/// 改行・タブ・連続スペース・末尾の `{` 直前空白を一括で潰す。
fn normalize_signature_whitespace(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn filter_exported_symbols(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
    file_path: Option<&str>,
) -> Vec<(String, String, String)> {
    use crate::models::symbol::SymbolKind;
    let source_str = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = source_str.lines().collect();

    // 同名別メソッドを区別するための enclosing container (class/struct/trait/interface) を収集。
    // メソッド/関数の range が container の range に内包される場合、qualname として
    // `Container.method` を使う（最も内側の container を優先）。
    let containers: Vec<&crate::models::symbol::Symbol> = syms
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Trait
                    | SymbolKind::Interface
                    | SymbolKind::Enum
            )
        })
        .collect();

    // Python 限定: 同一ファイル内の `unittest.TestCase` 派生クラスを fixed-point で解決する。
    // dead-code 経路でのみ使う想定だが、`exclude_framework_entrypoints` が true の場合に
    // 集合を構築すれば十分。
    let unittest_classes =
        if exclude_framework_entrypoints && lang_id == crate::language::LangId::Python {
            collect_python_unittest_classes(syms, root, source, lang_id)
        } else {
            std::collections::HashSet::new()
        };

    let mut result = Vec::new();
    for sym in syms {
        // モジュール宣言 (`pub mod foo;`) はファイル構成の整理であり、
        // 公開 API 面としての意味は薄い。dead-code / api.add 両経路で除外する
        // (Rust `mod`, Python の module、他言語の同等表現)。
        if matches!(sym.kind, SymbolKind::Module) {
            continue;
        }
        if !crate::engine::symbols::is_symbol_exported(root, source, lang_id, &sym.range) {
            continue;
        }
        // pub(crate), pub(super) 等はクレート内部APIなので除外
        let decl_line = lines.get(sym.range.start.line).unwrap_or(&"").trim();
        if decl_line.contains("pub(") {
            continue;
        }
        // C/C++ で実関数 body 内にネストした function_definition は、tree-sitter-cpp が
        // マクロ呼び出し (BOOST_FOREACH 等) を関数定義と誤パースした結果であることが多い。
        // 本物のトップレベル関数 / クラスメソッドではないため dead-code / API 変更検出の
        // どちらでも exported シンボルから除外する
        // (Issue #13: api_changes.modified が差分外の BOOST_FOREACH を拾う誤検出対策)。
        if matches!(
            lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
            && crate::engine::symbols::is_cpp_nested_function(root, &sym.range)
        {
            continue;
        }
        // C/C++ の前方宣言・opaque tag (本体を持たない struct/class/enum) は「定義」ではなく
        // 宣言であり、dead-code (未使用定義検出) や API 変更の対象にすべきではない。
        // `typedef struct st_mysql MYSQL;` の st_mysql (外部ライブラリの不透明構造体タグ) を
        // dead 誤検出する問題への対応 (Issue #11)。
        if matches!(
            lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(
            sym.kind,
            SymbolKind::Struct | SymbolKind::Class | SymbolKind::Enum
        ) && crate::engine::symbols::is_cpp_forward_declaration(root, &sym.range)
        {
            continue;
        }
        // Rust の `impl Trait for Type` 配下のメソッドは除外する。
        //   - dead-code 判定: trait dispatch 経由で呼ばれるため cross-file refs で caller を
        //     追跡できず、偽陽性になる。
        //   - API 変更検出: trait メソッドの実装は公開 item ではなく実装事実のため、個別の
        //     `on_ref` / `default` 等を api.add / api.rm にしない。必要であれば `impl Trait
        //     for Type` 単位で差分を扱うべきで、メソッド単位では扱わない。
        if exclude_trait_impls
            && lang_id == crate::language::LangId::Rust
            && crate::engine::symbols::is_trait_impl_method_rust(root, &sym.range)
        {
            continue;
        }
        // Kotlin/Java/Swift/TS/C# の `override` メソッドは親 interface/class の
        // メソッドを実装しているため、親型経由（Android の Listener callback 等）
        // で呼ばれる。cross-file refs では caller を追跡できず dead-code / api.add/rm
        // のいずれでも偽陽性になるため除外する。
        if exclude_trait_impls
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_override_method(root, source, lang_id, &sym.range)
        {
            continue;
        }
        // TS/JS の `constructor` メソッドは `new ClassName(...)` 構文で暗黙的に呼び出される。
        // 識別子レベルの cross-file refs では `constructor` 名を探しても見つからず、
        // クラスが利用されていても dead 判定される。クラス自体の dead 判定で十分なので、
        // constructor を独立した API/dead 候補から除外する。
        if matches!(sym.kind, SymbolKind::Method)
            && sym.name == "constructor"
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
        {
            continue;
        }
        // PHPUnit 規約のテストメソッド / テストクラス。PHP 限定。
        // `public function testXxx`, `setUp`, `tearDown`, `setUpBeforeClass`,
        // `tearDownAfterClass`, および `*Test` / `*TestCase` / `*IntegrationTest` /
        // `*FeatureTest` クラスは PHPUnit のランナーから自動で呼ばれる規約的シンボルで、
        // 識別子レベルの cross-file ref は発生しないが dead でもない。
        if is_phpunit_test_symbol(&sym.name, sym.kind, lang_id) {
            continue;
        }
        // PHP 擬似 enum (Java enum 風 static factory) パターン。PHP 限定。
        // `public static function FOO(): self { return new self('FOO'); }` 形式は
        // Laravel / DDD 系の AbstractValueObject 系で大量に存在し、
        // migration の文字列リテラル / DB 列値 / annotation reflection 経由で
        // 利用されるが識別子レベルの cross-file refs では caller が追跡できない。
        // dead-code の framework_entrypoints 除外と同じ意味合いで除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method)
            && crate::engine::symbols::is_php_pseudo_enum_method(
                root, source, &sym.range, &sym.name,
            )
        {
            continue;
        }
        // PHP の runtime annotation (`@TypeItem`, `@Route`, `@DataProvider`, `@dataProvider` 等) が
        // docstring に付いているメソッド / クラスは reflection 経由で動的に呼ばれるため
        // dead-code 候補から除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Class)
            && let Some(doc) = sym.doc.as_deref()
            && crate::engine::symbols::php_doc_has_runtime_annotation(doc)
        {
            continue;
        }
        // Python のフレームワーク登録デコレータ (Typer / Click / FastAPI / Flask /
        // pytest 等) で装飾された関数 / メソッド / クラスは、フレームワーク内部
        // レジストリ経由で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない。dead-code 判定では偽陽性源になるため除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Python
            && matches!(
                sym.kind,
                SymbolKind::Method | SymbolKind::Function | SymbolKind::Class
            )
            && crate::engine::symbols::has_framework_entrypoint_decorator_python(
                root, source, &sym.range,
            )
        {
            continue;
        }
        // JS/TS のフレームワーク DSL コールバック (WXT defineContentScript /
        // defineBackground、Vue defineComponent、Vite/Nuxt defineConfig 等) の
        // 引数オブジェクトメソッド (`main()`, `setup()` 等) は、フレームワーク内部
        // からビルド時連結で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない (Issue 2026-05-14-wxt-defineContentScript-main)。
        if exclude_framework_entrypoints
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_js_ts_framework_dsl_callback(root, source, &sym.range)
        {
            continue;
        }
        // Angular `@Component` / `@Directive` 装飾クラスの lifecycle hook メソッド
        // (`ngOnInit` / `ngAfterViewChecked` 等) は Angular ランタイムが change detection
        // サイクルで自動呼出するため、ユーザコード側に直接の caller が静的解析で見えない。
        // `implements AfterViewChecked` 等の interface 実装は Angular の呼出規約では
        // 不要なため判定材料にしない (Angular はメソッド名 + class decorator で hook を解決)。
        // GitLab issue #8 対応。
        if exclude_framework_entrypoints
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method)
            && crate::engine::symbols::is_js_ts_angular_lifecycle_hook(root, source, &sym.range)
        {
            continue;
        }
        // unittest / pytest のテスト規約シンボル。Python 限定。
        // `class Foo(unittest.TestCase):` 派生クラスとそのメソッド (`test_*`,
        // `setUp` 等)、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、
        // `conftest.py` 内の関数はテストランナーから動的 discover されるため、
        // 識別子レベルの cross-file refs では caller を追跡できない。
        if exclude_framework_entrypoints
            && is_python_test_symbol(
                &sym.name,
                sym.kind,
                lang_id,
                file_path,
                sym.container.as_deref(),
                &unittest_classes,
            )
        {
            continue;
        }
        let sig = extract_api_signature(sym, root, source, &lines, lang_id);
        let qualname = if matches!(sym.kind, SymbolKind::Method | SymbolKind::Function) {
            enclosing_container(sym, &containers)
                .map(|c| format!("{}.{}", c.name, sym.name))
                .unwrap_or_else(|| sym.name.clone())
        } else {
            sym.name.clone()
        };
        // qualname ベースでも最終チェック (例: `Foo.testBar` を PHP で除外)
        if is_phpunit_test_symbol(&qualname, sym.kind, lang_id) {
            continue;
        }
        // qualname ベースでも Python unittest 規約をチェック (`Foo.test_bar` 等)
        if exclude_framework_entrypoints
            && is_python_test_symbol(
                &qualname,
                sym.kind,
                lang_id,
                file_path,
                sym.container.as_deref(),
                &unittest_classes,
            )
        {
            continue;
        }
        result.push((qualname, format!("{:?}", sym.kind).to_lowercase(), sig));
    }
    result
}

/// 指定ファイル内で発生している全ての callee 名を集合として返す。
/// `extract_calls` と異なり、トップレベル呼び出し (関数本体外の `main()` や bash の
/// `timed "..."` 等) も含める。API 変更検出の内部ヘルパー判定用。
/// 失敗時（読み込み/パース不能）は空集合を返す。
fn extract_in_file_callees(dir: &str, file_path: &str) -> std::collections::HashSet<String> {
    let empty = std::collections::HashSet::new();
    let full_path = std::path::Path::new(dir).join(file_path);
    let Some(utf8_str) = full_path.to_str() else {
        return empty;
    };
    let utf8_path = camino::Utf8Path::new(utf8_str);
    let Ok(source) = parser::read_file(utf8_path) else {
        return empty;
    };
    let Ok(lang_id) = crate::language::LangId::from_path(utf8_path) else {
        return empty;
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return empty;
    };
    crate::engine::calls::extract_all_callees(tree.root_node(), &source, lang_id)
        .unwrap_or_default()
}

/// `qualname` (例: `Class.method` や bare name `foo`) が `callees` に含まれるかを判定する。
/// Python/Ruby など「obj.method()」形式で呼び出される言語では callee 側は bare name のみ
/// なので、qualname の末尾 (`.` 区切りの最後) でも判定する。
fn is_internally_connected(callees: &std::collections::HashSet<String>, qualname: &str) -> bool {
    if callees.contains(qualname) {
        return true;
    }
    if let Some(bare) = qualname.rsplit('.').next()
        && bare != qualname
        && callees.contains(bare)
    {
        return true;
    }
    false
}

/// 新規追加シンボル `name` が、同一 diff 内の別ファイル（`diff_new_paths`）から
/// 参照されているかを判定する。参照があれば「コミット内で完結して使用されている」
/// として api.add から除外する（pub struct の import や型参照が典型例）。
///
/// **`pub use` / `use` などの import/re-export 文中の参照は internal-use と数えない**
/// (Issue 2026-06-05-rust-api-add-private-module-reexport-edge-graph 対応)。
/// `pub use crate::wifi::found;` は内部利用ではなく公開エクスポートで、true で抑制すると
/// re-export が internal-use と誤認されて api.add から脱落する。実利用 (関数本体での
/// 呼び出し / 型注釈 / 値参照) があれば別の参照として出るためそちらで判定する。
///
/// `defining_file` / `diff_new_paths` は `dir` からの相対パスを想定する。
/// 参照検索に失敗した場合は false を返し、保守的に api.add に残す。
fn is_used_in_diff_paths(
    dir: &str,
    name: &str,
    defining_file: &str,
    diff_new_paths: &HashSet<String>,
) -> bool {
    use crate::models::reference::RefKind;
    // qualname (`Container.method`) の場合は bare name で参照検索する
    let search_name = bare_name(name);
    if search_name.is_empty() {
        return false;
    }
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(search_name, dir, None) else {
        return false;
    };
    let defining_path = std::path::Path::new(defining_file);
    // import/use 文が占める行集合を参照ファイルごとに 1 度だけ計算してキャッシュする。
    let mut import_lines_cache: std::collections::HashMap<
        String,
        std::collections::HashSet<usize>,
    > = std::collections::HashMap::new();
    refs_result.references.iter().any(|r| {
        if r.kind == Some(RefKind::Definition) {
            return false;
        }
        let ref_path = r.path.as_str();
        if std::path::Path::new(ref_path) == defining_path || !diff_new_paths.contains(ref_path) {
            return false;
        }
        // import/use 行は internal-use ではなく公開エクスポート経路なので除外する。
        if ref_is_import_line(r) {
            return false;
        }
        let import_lines = import_lines_cache
            .entry(ref_path.to_string())
            .or_insert_with(|| import_statement_lines_for_ref(dir, ref_path));
        if import_lines.contains(&r.line) {
            return false;
        }
        true
    })
}

/// `file_path` が属する Rust crate が binary-only (`src/lib.rs` を持たず外部から
/// `pub` シンボルへ到達できない構成) かを判定する。binary-only crate では `pub` は
/// クレート内モジュール境界の役割しか持たないため api.add の対象から除外する。
///
/// 判定方針: `file_path` (dir 相対) から祖先方向に遡って最も近い `Cargo.toml` を
/// 見つけ、そのディレクトリで `src/lib.rs` が存在せず、かつ `Cargo.toml` に `[lib]`
/// セクションも書かれていなければ binary-only とみなす。`[lib] path = "..."` のような
/// custom path で lib crate を構成しているケースを誤って binary-only と判定しないよう、
/// TOML の `[lib]` セクション存在も判定に含める。`Cargo.toml` のパースに失敗した場合は
/// 保守的に false (binary-only ではない) を返す。Rust ファイル以外や `Cargo.toml` が
/// 見つからない場合も false を返す。
fn is_binary_only_rust_crate(dir: &str, file_path: &str) -> bool {
    let path = std::path::Path::new(file_path);
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    let full = std::path::Path::new(dir).join(file_path);
    let dir_canonical = std::fs::canonicalize(dir).ok();
    let mut current = full.parent();
    while let Some(d) = current {
        let cargo_toml = d.join("Cargo.toml");
        if cargo_toml.is_file() {
            if d.join("src").join("lib.rs").is_file() {
                return false;
            }
            // Cargo.toml に `[lib]` セクションがあれば custom path の lib crate。
            // パース失敗時は保守的に lib crate 扱い (false = binary-only ではない)。
            let Ok(text) = std::fs::read_to_string(&cargo_toml) else {
                return false;
            };
            return !cargo_toml_text_declares_lib(&text);
        }
        // dir より上には探索しない
        if let (Some(root), Ok(canon)) = (dir_canonical.as_ref(), std::fs::canonicalize(d))
            && canon == *root
        {
            return false;
        }
        current = d.parent();
    }
    false
}

/// `api.rm` 側専用: `base` リビジョン時点での crate type を判定する。
///
/// 新ツリーで `src/lib.rs` を削除した、または `Cargo.toml` の `[lib]` セクションを
/// 同一 diff で消したケースで、旧公開 API の削除まで誤って `api.rm` から除外しないため、
/// `git show` で旧側の `Cargo.toml` / `src/lib.rs` を取得して判定する。
///
/// 判定方針:
/// - `file_path` (dir 相対) の祖先方向に向けて、`base` リビジョンに存在する最も近い
///   `Cargo.toml` を探す
/// - その `Cargo.toml` ディレクトリで base 側に `src/lib.rs` があれば library crate
/// - `Cargo.toml` を TOML パースし `[lib]` セクションがあれば library crate
/// - いずれの判定にも失敗 / 該当しない場合 = binary-only
///
/// 失敗時は保守的に `false` (library crate 扱い) を返し、`api.rm` を抑制しない方向に倒す。
fn is_binary_only_rust_crate_at_base(dir: &str, base: &str, file_path: &str) -> bool {
    let path = std::path::Path::new(file_path);
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    if validate_git_revision(base, "--base").is_err() {
        return false;
    }
    // dir 相対パスの祖先を順に辿り、最初に base 時点で存在した Cargo.toml を採用する。
    let mut ancestor: Option<&std::path::Path> = path.parent();
    while let Some(rel_dir) = ancestor {
        let cargo_rel = if rel_dir.as_os_str().is_empty() {
            std::path::PathBuf::from("Cargo.toml")
        } else {
            rel_dir.join("Cargo.toml")
        };
        let cargo_rel_str = cargo_rel.to_string_lossy().to_string();
        if validate_git_revision(&cargo_rel_str, "diff file path").is_err() {
            return false;
        }
        let cargo_output = std::process::Command::new("git")
            .args(["show", &format!("{base}:{cargo_rel_str}")])
            .current_dir(dir)
            .output();
        if let Ok(out) = cargo_output
            && out.status.success()
        {
            // 同 crate root の base 側 src/lib.rs 存在を git show で判定
            let lib_rel = if rel_dir.as_os_str().is_empty() {
                std::path::PathBuf::from("src/lib.rs")
            } else {
                rel_dir.join("src/lib.rs")
            };
            let lib_rel_str = lib_rel.to_string_lossy().to_string();
            if validate_git_revision(&lib_rel_str, "diff file path").is_err() {
                return false;
            }
            let lib_output = std::process::Command::new("git")
                .args(["show", &format!("{base}:{lib_rel_str}")])
                .current_dir(dir)
                .output();
            if matches!(lib_output, Ok(ref o) if o.status.success()) {
                return false;
            }
            let Ok(text) = std::str::from_utf8(&out.stdout) else {
                return false;
            };
            return !cargo_toml_text_declares_lib(text);
        }
        // ancestor を一つ上に
        match rel_dir.parent() {
            Some(parent) => ancestor = Some(parent),
            None => break,
        }
        if ancestor.is_some_and(|p| p.as_os_str().is_empty()) {
            // ルート直下まで来たので最後にもう一度だけ Cargo.toml チェックする
            let last = std::path::PathBuf::from("Cargo.toml");
            let cargo_rel_str = last.to_string_lossy().to_string();
            let cargo_output = std::process::Command::new("git")
                .args(["show", &format!("{base}:{cargo_rel_str}")])
                .current_dir(dir)
                .output();
            if let Ok(out) = cargo_output
                && out.status.success()
            {
                let lib_output = std::process::Command::new("git")
                    .args(["show", &format!("{base}:src/lib.rs")])
                    .current_dir(dir)
                    .output();
                if matches!(lib_output, Ok(ref o) if o.status.success()) {
                    return false;
                }
                let Ok(text) = std::str::from_utf8(&out.stdout) else {
                    return false;
                };
                return !cargo_toml_text_declares_lib(text);
            }
            break;
        }
    }
    false
}

/// `api.rm` 判定用: 旧 (base) 側で削除されたシンボル `symbol_name` が「外部公開 API 面の外」に
/// あるかを返す。bin-only crate の `pub`、または crate-private module (`mod foo`、`pub mod` 経路で
/// 到達不能) 配下の `pub` は crate 外から構造的に到達できないため、削除されても破壊的変更ではない。
///
/// ただし private module 配下でも、別の public-reachable module から `pub use` で re-export 公開
/// されている (`pub mod prelude;` + prelude.rs に `pub use crate::wifi::found;` 等) 場合は外部公開
/// API 面に含まれるため抑制しない。`reexport_cache` で base+crate 単位の re-export index を一度だけ
/// 構築する。`api.add` (new 側) / `api.mod` (old/new 両側) の private module 抑制と対称に base 側で判定する。
fn is_rust_old_symbol_outside_public_api_surface(
    dir: &str,
    base: &str,
    old_path: &str,
    symbol_name: &str,
    reexport_cache: &mut RustBaseReexportCache,
) -> bool {
    if is_binary_only_rust_crate_at_base(dir, base, old_path) {
        return true;
    }
    // symbol が inline `mod_item` 内 (`mod foo { pub fn symbol() }` 形式) で定義されている
    // 場合、ファイルパス由来の module_segments とずれて edge graph seed が誤合致する。
    // 範囲限定 fail-closed: false negative を防ぐため `api.rm` 抑制を諦め symbol を残す
    // (Issue 2026-06-05-rust-api-add-private-module-reexport-edge-graph の codex 指摘)。
    if rust_symbol_is_inside_inline_mod(
        RustSourceTree::Base { rev: base },
        dir,
        old_path,
        symbol_name,
    ) {
        return false;
    }
    // re-export を考慮しない raw private 判定。public-reachable / 判定不能なら api.rm を残す。
    let Some(private) = rust_private_module_info(RustSourceTree::Base { rev: base }, dir, old_path)
    else {
        return false;
    };
    // index 構築に失敗したら api.rm を残す (false negative 回避優先)。
    let Some(index) = reexport_cache.index_for(dir, base, &private) else {
        return false;
    };
    !index.exposes_symbol(&private, symbol_name)
}

/// base 側 crate の private module 情報 (re-export は考慮しない raw 判定の結果)。
struct RustPrivateModuleInfo {
    crate_root_rel: std::path::PathBuf,
    src_root_rel: std::path::PathBuf,
    /// `file_path` の src 相対モジュールパス (例: `[wifi]` / `[wifi, detector]`)。
    module_segments: Vec<String>,
}

/// base 側で `file_path` (dir 相対) の private module 情報を構築する。re-export は考慮しない
/// (index 側で扱う)。public-reachable (全 `pub mod`) なら `None`、判定不能 (`#[path]` / inline mod /
/// 宣言未検出 / モジュールファイル解決不能) も `None` を返し、呼び出し側で api.rm を残す方向に倒す。
/// `file_path` (dir 相対) の Rust source が属する private module の情報を返す。
/// `RustSourceTree::Base { rev }` なら base リビジョン、`RustSourceTree::Worktree` なら working
/// tree のソースを読む (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。
///
/// lib.rs から mod 宣言チェーンを辿り、最初に private (`mod` 修飾なし) だった prefix を含む
/// `RustPrivateModuleInfo` を返す。`#[path]` 属性 / inline mod / 宣言未検出は `None` を返して
/// 上流で fail-closed する。全 `pub mod` で到達可能なら `None` (public-reachable)。
fn rust_private_module_info(
    source: RustSourceTree<'_>,
    dir: &str,
    file_path: &str,
) -> Option<RustPrivateModuleInfo> {
    use std::path::{Path, PathBuf};
    let rel = Path::new(file_path);
    if rel.extension().and_then(|s| s.to_str()) != Some("rs") {
        return None;
    }
    let canonical_dir = std::fs::canonicalize(dir).ok()?;
    let abs = canonical_dir.join(rel);
    let mut crate_root: Option<PathBuf> = None;
    let mut anc = abs.parent();
    while let Some(d) = anc {
        if d.join("Cargo.toml").is_file() {
            crate_root = Some(d.to_path_buf());
            break;
        }
        if d == canonical_dir {
            break;
        }
        anc = d.parent();
    }
    let crate_root = crate_root?;
    let src_dir = crate_root.join("src");
    if !src_dir.join("lib.rs").is_file() {
        return None;
    }
    let rel_to_src = abs.strip_prefix(&src_dir).ok()?;
    let segments = module_path_segments(rel_to_src);
    if segments.is_empty() {
        return None;
    }
    let crate_root_rel = crate_root.strip_prefix(&canonical_dir).ok()?.to_path_buf();
    let src_root_rel = crate_root_rel.join("src");
    let mut current_rel = PathBuf::from("lib.rs");
    for (idx, seg) in segments.iter().enumerate() {
        let module_source = read_rust_module_source(source, dir, &crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        match find_mod_decl_visibility(tree.root_node(), &module_source, seg) {
            Some(true) => {
                let parent = current_rel
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let as_mod = parent.join(seg).join("mod.rs");
                let as_file = parent.join(format!("{seg}.rs"));
                if src_dir.join(&as_mod).is_file() {
                    current_rel = as_mod;
                } else if src_dir.join(&as_file).is_file() {
                    current_rel = as_file;
                } else {
                    return None;
                }
            }
            Some(false) => {
                let _ = idx;
                return Some(RustPrivateModuleInfo {
                    crate_root_rel,
                    src_root_rel,
                    module_segments: segments,
                });
            }
            None => return None,
        }
    }
    None
}

/// `api.add` 判定用: 新 (working tree) 側で新規追加されたシンボル `symbol_name` が
/// 「外部公開 API 面の外」にあるかを返す。bin-only crate / crate-private module (`mod foo`、
/// `pub mod` 経路で到達不能) 配下の `pub` は外部到達できないため、追加されても外部 API 面で
/// はない。ただし private module でも別の public-reachable module から `pub use` で re-export
/// 公開されている場合は外部 API 面に含めるため、edge graph + 固定点伝播で判定する。
///
/// `api.rm` 側 (`is_rust_old_symbol_outside_public_api_surface`) と対称の処理を、base 側でなく
/// working tree 側に行う。`reexport_cache` は new 側 crate 単位で再利用する。
fn is_rust_new_symbol_outside_public_api_surface(
    dir: &str,
    new_path: &str,
    symbol_name: &str,
    reexport_cache: &mut RustWorktreeReexportCache,
) -> bool {
    if is_binary_only_rust_crate(dir, new_path) {
        return true;
    }
    // symbol が inline `mod_item` 内で定義されている場合、ファイルパス由来の module_segments
    // とずれて edge graph seed が誤合致するため、fail-closed で `api.add` 抑制を諦める。
    if rust_symbol_is_inside_inline_mod(RustSourceTree::Worktree, dir, new_path, symbol_name) {
        return false;
    }
    // raw private 判定 (re-export 考慮なし)。public-reachable / 判定不能なら api.add を残す。
    let Some(private) = rust_private_module_info(RustSourceTree::Worktree, dir, new_path) else {
        return false;
    };
    let Some(index) = reexport_cache.index_for(dir, &private) else {
        return false; // index 構築失敗 → fail-closed (api.add を残す)
    };
    !index.exposes_symbol(&private, symbol_name)
}

/// ファイル AST を walk して `symbol_name` の定義が inline `mod_item` (`mod foo { ... }`)
/// の中にあるかを判定する。working tree 側。`mod_item` を見つけたら、その body 内に
/// 同名 identifier の定義 (function_item / struct_item / enum_item / type_alias 等の
/// name field) があるかを確認する。複数経路に同名がある場合は保守的に true (=fail-closed
/// 側に倒し抑制しない方向)。検出失敗・parse 失敗・ファイル読み込み失敗時は false。
/// ファイルソース (`source` 経由) を Rust として parse し、`symbol_name` の定義が inline
/// `mod_item` body 内にあるかを判定する (リファクタ Step 3: `_at_base` / `_at_worktree` の
/// 本体統合)。読み込み / parse 失敗時は false (= 抑制しない / shadow なし扱い)。
fn rust_symbol_is_inside_inline_mod(
    source: RustSourceTree<'_>,
    dir: &str,
    file_path: &str,
    symbol_name: &str,
) -> bool {
    let source_bytes = match source {
        RustSourceTree::Worktree => {
            let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
                return false;
            };
            let full = canonical_dir.join(file_path);
            match std::fs::read(&full) {
                Ok(s) => s,
                Err(_) => return false,
            }
        }
        RustSourceTree::Base { rev } => {
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(file_path, "diff file path").is_err()
            {
                return false;
            }
            match std::process::Command::new("git")
                .args(["show", &format!("{rev}:{file_path}")])
                .current_dir(dir)
                .output()
            {
                Ok(o) if o.status.success() => o.stdout,
                _ => return false,
            }
        }
    };
    rust_source_has_symbol_in_inline_mod(&source_bytes, symbol_name)
}

/// 共通ロジック: source を Rust として parse し、inline `mod_item` の body 内に
/// `symbol_name` の定義 (name field が一致する `function_item` / `struct_item` /
/// `enum_item` / `type_item` / `const_item` / `static_item` / `trait_item` / `mod_item`) が
/// あるか再帰探索する。
fn rust_source_has_symbol_in_inline_mod(source: &[u8], symbol_name: &str) -> bool {
    let bare = bare_name(symbol_name);
    let tree = match parser::parse_source(source, crate::language::LangId::Rust) {
        Ok(t) => t,
        Err(_) => return false,
    };
    walk_for_inline_mod_containing(tree.root_node(), source, bare, false)
}

/// 再帰 walk: `inside_inline_mod=true` のスコープに symbol 定義があれば true。
/// `mod_item` の body (declaration_list) に入ったら `inside_inline_mod=true` で再帰する。
fn walk_for_inline_mod_containing(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    symbol_name: &str,
    inside_inline_mod: bool,
) -> bool {
    let kind = node.kind();
    // 対象シンボル定義かを判定 (name field を持つ各種 item)
    if inside_inline_mod
        && matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "trait_item"
                | "mod_item"
                | "union_item"
        )
        && let Some(name_node) = node.child_by_field_name("name")
        && name_node.utf8_text(source).map(str::trim) == Ok(symbol_name)
    {
        return true;
    }
    // 子 node を再帰 walk。`mod_item` の declaration_list (body) に入ったら
    // inside_inline_mod=true で潜る。`mod foo;` (宣言のみ) は body が無いので追加判定なし。
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let next_inside = if kind == "mod_item" && child.kind() == "declaration_list" {
            true
        } else {
            inside_inline_mod
        };
        if walk_for_inline_mod_containing(child, source, symbol_name, next_inside) {
            return true;
        }
    }
    false
}

/// working tree 側で `file_path` (dir 相対) の private module 情報を構築する。`rust_private_module_info_at_base`
/// working tree から `<crate_root_rel>/src/<module_rel>` のソースを読み取る。`read_rust_module_source_at_base`
/// の worktree 版。failures は `None` を返し、呼び出し側で `api.add` 抑制を諦める。
/// Rust crate のソースツリーをどこから読むかを表す抽象化。
///
/// `Worktree` は `std::fs` 経由で working tree を直接読み、`Base { rev }` は `git show <rev>:<path>` /
/// `git ls-tree <rev>` 経由で base リビジョンを読む。`read_rust_module_source` / `collect_rust_rs_files` /
/// `RustReexportCache` の API に渡して I/O 差分を吸収する (リファクタ Step 1: I/O 抽象化、
/// 別 Issue `2026-06-06-refactor-rust-private-module-helpers-with-source-tree-enum.md` 対応)。
#[derive(Clone, Copy, Debug)]
enum RustSourceTree<'a> {
    Worktree,
    Base { rev: &'a str },
}

/// `crate_root_rel`/src/`module_rel` を `source` 経由で読む。Worktree なら `std::fs::read`、
/// Base なら `git show <rev>:<crate_root_rel>/src/<module_rel>`。失敗時は `None`。
fn read_rust_module_source(
    source: RustSourceTree<'_>,
    dir: &str,
    crate_root_rel: &std::path::Path,
    module_rel: &std::path::Path,
) -> Option<Vec<u8>> {
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let full = canonical_dir
                .join(crate_root_rel)
                .join("src")
                .join(module_rel);
            std::fs::read(full).ok()
        }
        RustSourceTree::Base { rev } => {
            let full_rel = crate_root_rel.join("src").join(module_rel);
            let full_rel_str = full_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(full_rel_str, "diff file path").is_err()
            {
                return None;
            }
            let out = std::process::Command::new("git")
                .args(["show", &format!("{rev}:{full_rel_str}")])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            Some(out.stdout)
        }
    }
}

/// `src_root_rel` 配下の `.rs` ファイル列 (repo 相対) を `source` 経由で取得する。
/// Worktree なら `ignore::WalkBuilder`、Base なら `git ls-tree -r --name-only`。
fn collect_rust_rs_files(
    source: RustSourceTree<'_>,
    dir: &str,
    src_root_rel: &std::path::Path,
) -> Option<Vec<std::path::PathBuf>> {
    match source {
        RustSourceTree::Worktree => {
            use ignore::WalkBuilder;
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let src_full = canonical_dir.join(src_root_rel);
            if !src_full.is_dir() {
                return None;
            }
            let mut files = Vec::new();
            for entry in WalkBuilder::new(&src_full).hidden(false).build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let rel = match path.strip_prefix(&canonical_dir) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                files.push(rel);
            }
            Some(files)
        }
        RustSourceTree::Base { rev } => {
            let src_str = src_root_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(src_str, "diff file path").is_err()
            {
                return None;
            }
            let out = std::process::Command::new("git")
                .args(["ls-tree", "-r", "--name-only", rev, "--", src_str])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = std::str::from_utf8(&out.stdout).ok()?;
            Some(
                text.lines()
                    .filter(|l| l.ends_with(".rs"))
                    .map(std::path::PathBuf::from)
                    .collect(),
            )
        }
    }
}

/// 統合 cache キー (リファクタ Step 2: cache 統合)。`rev = None` で working tree、
/// `rev = Some(<rev>)` で base リビジョンを表す。型で意図を明確化することで、
/// `(Option<String>, PathBuf)` の生 tuple よりも事故りにくい。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct RustSourceTreeKey {
    rev: Option<String>,
    crate_root_rel: std::path::PathBuf,
}

impl RustSourceTreeKey {
    fn from_source(source: RustSourceTree<'_>, crate_root_rel: std::path::PathBuf) -> Self {
        let rev = match source {
            RustSourceTree::Worktree => None,
            RustSourceTree::Base { rev } => Some(rev.to_string()),
        };
        Self {
            rev,
            crate_root_rel,
        }
    }
}

/// base / worktree 統合 re-export cache (リファクタ Step 2)。
/// `RustBaseReexportCache` / `RustWorktreeReexportCache` の本体としても使用される
/// (これら 2 cache は外部 API 維持のため薄い wrapper として残り、内部は本 cache に転送する)。
#[derive(Default)]
struct RustReexportCache {
    by_key: std::collections::HashMap<RustSourceTreeKey, Option<RustPubUseIndex>>,
}

impl RustReexportCache {
    fn index_for(
        &mut self,
        source: RustSourceTree<'_>,
        dir: &str,
        info: &RustPrivateModuleInfo,
    ) -> Option<&RustPubUseIndex> {
        let key = RustSourceTreeKey::from_source(source, info.crate_root_rel.clone());
        self.by_key
            .entry(key)
            .or_insert_with(|| match source {
                RustSourceTree::Worktree | RustSourceTree::Base { .. } => {
                    collect_rust_pub_use_index(source, dir, info)
                }
            })
            .as_ref()
    }
}

/// working tree 用 re-export cache。`api.add` 経路から呼ばれる外部 API を維持しつつ、
/// 内部は統合 `RustReexportCache` に転送する (リファクタ Step 2: cache 統合)。
#[derive(Default)]
struct RustWorktreeReexportCache {
    inner: RustReexportCache,
}

impl RustWorktreeReexportCache {
    fn index_for(&mut self, dir: &str, info: &RustPrivateModuleInfo) -> Option<&RustPubUseIndex> {
        self.inner.index_for(RustSourceTree::Worktree, dir, info)
    }
}

/// base+crate 単位で `pub use` re-export index を一度だけ構築するキャッシュ。
/// `api.rm` / `api.mod` 経路から呼ばれる外部 API を維持しつつ、内部は統合 `RustReexportCache`
/// に転送する (リファクタ Step 2: cache 統合)。
#[derive(Default)]
struct RustBaseReexportCache {
    inner: RustReexportCache,
}

impl RustBaseReexportCache {
    fn index_for(
        &mut self,
        dir: &str,
        base: &str,
        info: &RustPrivateModuleInfo,
    ) -> Option<&RustPubUseIndex> {
        self.inner
            .index_for(RustSourceTree::Base { rev: base }, dir, info)
    }
}

/// base 側 crate の public-reachable な module 群から集めた `pub use` re-export ターゲット。
/// re-export edge graph + public-reachable module 集合 + 逆引き map。
/// `collect_rust_pub_use_index_at_base` で base 側 crate 全体を 1 度走査して構築し、
/// `exposes_symbol` で削除シンボルから固定点伝播して公開到達性を判定する。
struct RustPubUseIndex {
    edges: Vec<RustPubUseEdge>,
    /// 外部から `crate::<path>` で到達可能な module 集合 (root = `[]`、`pub mod` のみで到達)。
    public_modules: std::collections::HashSet<Vec<String>>,
    /// `(target_module, target_item)` → Named edge index。Named 伝播の逆引き。
    named_by_target: std::collections::HashMap<RustExportKey, Vec<usize>>,
    /// `target_module` → Wildcard edge index。Wildcard 伝播の逆引き。
    wildcard_by_target_module: std::collections::HashMap<Vec<String>, Vec<usize>>,
}

/// 「ある module でこの名前がエクスポートされている」を表す key。固定点計算の単位。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RustExportKey {
    module: Vec<String>,
    name: String,
}

/// `pub use` から生成される re-export edge。Named は `source_module::exported_name` が
/// `target_module::target_item` を指す。alias の場合 `exported_name = alias`、`target_item = 元名`。
/// Wildcard は `source_module::* = target_module::*` で名前ごとに伝播する。
#[derive(Clone, Debug)]
enum RustPubUseEdge {
    Named {
        source_module: Vec<String>,
        exported_name: String,
        target_module: Vec<String>,
        target_item: String,
    },
    Wildcard {
        source_module: Vec<String>,
        target_module: Vec<String>,
    },
}

impl RustPubUseIndex {
    /// `info` の private module 配下の `symbol_name` が外部公開 API として到達可能かを返す。
    /// 削除シンボルを seed として live export 集合を固定点伝播し、live ∩ public_modules ≠ ∅ なら true。
    fn exposes_symbol(&self, info: &RustPrivateModuleInfo, symbol_name: &str) -> bool {
        let item = rust_reexport_item_name(symbol_name).to_string();
        let seed = RustExportKey {
            module: info.module_segments.clone(),
            name: item,
        };
        self.propagate_live_exports(seed)
            .into_iter()
            .any(|key| self.public_modules.contains(&key.module))
    }

    /// 削除 seed から逆向きに live export を BFS で伝播。HashSet で重複を防いで循環で停止する。
    fn propagate_live_exports(
        &self,
        seed: RustExportKey,
    ) -> std::collections::HashSet<RustExportKey> {
        use std::collections::{HashSet, VecDeque};
        let mut live: HashSet<RustExportKey> = HashSet::new();
        let mut queue: VecDeque<RustExportKey> = VecDeque::new();
        live.insert(seed.clone());
        queue.push_back(seed);
        while let Some(key) = queue.pop_front() {
            if let Some(edge_ids) = self.named_by_target.get(&key) {
                for &idx in edge_ids {
                    if let RustPubUseEdge::Named {
                        source_module,
                        exported_name,
                        ..
                    } = &self.edges[idx]
                    {
                        let next = RustExportKey {
                            module: source_module.clone(),
                            name: exported_name.clone(),
                        };
                        if live.insert(next.clone()) {
                            queue.push_back(next);
                        }
                    }
                }
            }
            if let Some(edge_ids) = self.wildcard_by_target_module.get(&key.module) {
                for &idx in edge_ids {
                    if let RustPubUseEdge::Wildcard { source_module, .. } = &self.edges[idx] {
                        let next = RustExportKey {
                            module: source_module.clone(),
                            name: key.name.clone(),
                        };
                        if live.insert(next.clone()) {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }
        live
    }
}

/// re-export item 名。Rust の method は `Container.method` qualname で出るが re-export 対象 item は
/// container の `Container`。free function / struct 等は bare name。
fn rust_reexport_item_name(name: &str) -> &str {
    if let Some((container, _method)) = name.split_once('.') {
        container
    } else {
        bare_name(name)
    }
}

/// base 側 crate の src/ 配下を全 .rs 走査して `pub use` を edge として集め、public-reachable module
/// 集合と逆引き map を構築する。public-reachable filter は collect 段階では外し (private module 内の
/// pub use も root から `pub use private::x` されれば公開になり得るため)、最終判定は `exposes_symbol`
/// の固定点伝播で行う。`git ls-tree` / `git show` / parse / path 解決のいずれかで判定不能になったら
/// `None` を返して `api.rm` を残す (false negative 回避)。
/// Rust crate の src/ 配下を `source` 経由で全走査し、`pub use` re-export edge graph と
/// public-reachable module 集合を構築する (リファクタ Step 3: `_at_base` / `_at_worktree` の
/// 本体統合)。public-reachable filter は collect 段階では外し、最終判定は
/// `exposes_symbol` の固定点伝播で行う。`ls-tree` / `read` / parse / path 解決のいずれかが
/// 失敗したら `None` を返す (`api.rm` / `api.add` を残す方向、false negative 回避)。
fn collect_rust_pub_use_index(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<RustPubUseIndex> {
    let files = collect_rust_rs_files(source, dir, &info.src_root_rel)?;
    let mut edges: Vec<RustPubUseEdge> = Vec::new();
    for file in files {
        let Ok(rel_to_src) = file.strip_prefix(&info.src_root_rel) else {
            continue;
        };
        let module_path = module_path_segments(rel_to_src);
        let file_source = read_rs_blob(source, dir, &file)?;
        let tree = parser::parse_source(&file_source, crate::language::LangId::Rust).ok()?;
        collect_pub_use_edges(tree.root_node(), &file_source, &module_path, &mut edges)?;
    }
    let mut named_by_target: std::collections::HashMap<RustExportKey, Vec<usize>> =
        std::collections::HashMap::new();
    let mut wildcard_by_target_module: std::collections::HashMap<Vec<String>, Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, edge) in edges.iter().enumerate() {
        match edge {
            RustPubUseEdge::Named {
                target_module,
                target_item,
                ..
            } => {
                let key = RustExportKey {
                    module: target_module.clone(),
                    name: target_item.clone(),
                };
                named_by_target.entry(key).or_default().push(idx);
            }
            RustPubUseEdge::Wildcard { target_module, .. } => {
                wildcard_by_target_module
                    .entry(target_module.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
    let public_modules = public_reachable_modules(source, dir, info)?;
    Some(RustPubUseIndex {
        edges,
        public_modules,
        named_by_target,
        wildcard_by_target_module,
    })
}

/// `src/` 配下の `.rs` ファイル (file は dir 相対) を `source` 経由で読む。
/// Worktree なら `std::fs::read(<canonical_dir>/<file>)`、Base なら `git show <rev>:<file>`。
fn read_rs_blob(source: RustSourceTree<'_>, dir: &str, file: &std::path::Path) -> Option<Vec<u8>> {
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let abs = canonical_dir.join(file);
            std::fs::read(abs).ok()
        }
        RustSourceTree::Base { rev } => {
            let file_str = file.to_str()?;
            read_git_blob_at_base(dir, rev, file_str)
        }
    }
}

/// `source` 経由で lib.rs (crate root) から `pub mod` 経路 (制限なし pub) で到達できる
/// module 集合を構築する (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。root `[]`
/// は常に含む。inline `pub mod foo { ... }` も再帰的に拾う。`mod foo;` (制限なし pub なし) は
/// 除外する。判定不能 (`#[path]` / モジュールファイル解決失敗 / 解析失敗) は `None` を返し、
/// 呼出元で api.rm を残す (fail-closed)。
fn public_reachable_modules(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<std::collections::HashSet<Vec<String>>> {
    use std::collections::HashSet;
    use std::path::PathBuf;
    let mut result: HashSet<Vec<String>> = HashSet::new();
    result.insert(Vec::new());
    let mut frontier: Vec<(Vec<String>, PathBuf)> = vec![(Vec::new(), PathBuf::from("lib.rs"))];
    while let Some((segments, current_rel)) = frontier.pop() {
        let module_source =
            read_rust_module_source(source, dir, &info.crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        collect_public_pub_mods(
            source,
            tree.root_node(),
            &module_source,
            dir,
            info,
            &segments,
            &current_rel,
            &mut result,
            &mut frontier,
        )?;
    }
    Some(result)
}

/// `lib.rs` / 親モジュールファイルから子の `pub mod` を `source` 経由で再帰的に集める。
/// inline body は同じファイル内で walk 続行、宣言のみは次のモジュールファイルを resolve して
/// frontier に積む (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。
#[allow(clippy::too_many_arguments)]
fn collect_public_pub_mods(
    source: RustSourceTree<'_>,
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    dir: &str,
    info: &RustPrivateModuleInfo,
    current_segments: &[String],
    current_file_rel: &std::path::Path,
    result: &mut std::collections::HashSet<Vec<String>>,
    frontier: &mut Vec<(Vec<String>, std::path::PathBuf)>,
) -> Option<()> {
    use std::path::Path;
    match node.kind() {
        "mod_item" => {
            if rust_mod_item_has_path_attribute(node, source_bytes) {
                return None;
            }
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source_bytes).ok())
                .map(str::to_string)?;
            let is_pub = rust_use_declaration_is_pub(node, source_bytes);
            if !is_pub {
                return Some(());
            }
            let mut child_segments = current_segments.to_vec();
            child_segments.push(name.clone());
            result.insert(child_segments.clone());
            let mut has_inline_body = false;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    has_inline_body = true;
                    let mut inner_cursor = child.walk();
                    for inner in child.named_children(&mut inner_cursor) {
                        collect_public_pub_mods(
                            source,
                            inner,
                            source_bytes,
                            dir,
                            info,
                            &child_segments,
                            current_file_rel,
                            result,
                            frontier,
                        )?;
                    }
                }
            }
            if !has_inline_body {
                let parent_dir = current_file_rel
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let as_mod = parent_dir.join(&name).join("mod.rs");
                let as_file = parent_dir.join(format!("{name}.rs"));
                if read_rust_module_source(source, dir, &info.crate_root_rel, &as_mod).is_some() {
                    frontier.push((child_segments, as_mod));
                } else if read_rust_module_source(source, dir, &info.crate_root_rel, &as_file)
                    .is_some()
                {
                    frontier.push((child_segments, as_file));
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_public_pub_mods(
                    source,
                    child,
                    source_bytes,
                    dir,
                    info,
                    current_segments,
                    current_file_rel,
                    result,
                    frontier,
                )?;
            }
        }
    }
    Some(())
}

/// `git show <base>:<file>` で blob を取る (file は repo 相対)。
fn read_git_blob_at_base(dir: &str, base: &str, file: &str) -> Option<Vec<u8>> {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file, "diff file path").is_err()
    {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// AST を走査し `use_declaration` ノードから `pub use` re-export edge を集める。
///
/// - `current_module`: lib.rs 起点の current source 所属モジュール (super:: 解決 + source_module に使う)
/// - 戻り値 `None` = 「判定不能」 (解決不能な super:: や不正な use tree)。呼出元は index 全体を
///   `None` にして api.rm を残す (false negative より false positive を優先する fail-closed 方針)
///
/// **注**: Step A の `collect_pub_use_targets` から「inline_private_depth による pub use 除外」を外した。
/// 非 pub inline mod 配下の `pub use` でも、root から `pub use private_mod::x` されれば外部公開
/// 経路になり得るため。最終判定は `RustPubUseIndex::exposes_symbol` の固定点伝播で行う。
fn collect_pub_use_edges(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    current_module: &[String],
    edges: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    match node.kind() {
        "use_declaration" => {
            if !rust_use_declaration_is_pub(node, source) {
                return Some(());
            }
            let argument = node.child_by_field_name("argument")?;
            expand_rust_use_tree_edges_ast(
                argument,
                source,
                &[],
                current_module,
                current_module,
                None,
                edges,
            )?;
        }
        "mod_item" => {
            // #[path = "..."] でファイル名と module 名がずれる場合、source_module の解決を保守的に
            // 諦めて index 全体を None にする (codex Warning #3 対応、fail-closed)。
            if rust_mod_item_has_path_attribute(node, source) {
                return None;
            }
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string);
            let mut next_module = current_module.to_vec();
            if let Some(seg) = name {
                next_module.push(seg);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_pub_use_edges(child, source, &next_module, edges)?;
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_pub_use_edges(child, source, current_module, edges)?;
            }
        }
    }
    Some(())
}

/// `mod_item` の直前の同一スコープ sibling に `#[path = "..."]` attribute があるかを返す。
/// tree-sitter-rust では attribute_item と mod_item は親 (source_file / declaration_list) の
/// 子として **隣接 sibling** に並ぶため、prev_sibling を逆方向に辿って attribute_item を集める。
/// `#[path]` が見つかったら true。
fn rust_mod_item_has_path_attribute(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if sib.kind() != "attribute_item" {
            break; // 連続する attribute_item は積み上がるが、他の宣言が出たら終了
        }
        if attribute_item_is_path(sib, source) {
            return true;
        }
        prev = sib.prev_named_sibling();
    }
    false
}

/// `attribute_item` の中身が `#[path = ...]` か判定する。
fn attribute_item_is_path(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attr_item.walk();
    for child in attr_item.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            // attribute の最初の identifier 子 (= attribute path) の text を見る。
            let mut inner = child.walk();
            for c in child.named_children(&mut inner) {
                if c.kind() == "identifier" || c.kind() == "scoped_identifier" {
                    return c.utf8_text(source).map(str::trim) == Ok("path");
                }
            }
        }
    }
    false
}

/// `use_declaration` / `mod_item` ノードが「制限なし `pub`」 (`pub(crate)` / `pub(super)` 等の
/// 制限付きや非 pub を除く) かを返す。`visibility_modifier` 子ノードのテキストを厳密に `"pub"` で照合する。
fn rust_use_declaration_is_pub(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(source).map(str::trim) == Ok("pub");
        }
    }
    false
}

/// 構造的に AST を walk して `pub use` re-export ターゲットを抽出する (whitespace / コメント非依存)。
///
/// `argument` ノードは tree-sitter-rust の以下のいずれかになる:
/// - `identifier`: 単一名 `pub use Foo;` (この crate root の Foo を再エクスポート)
/// - `scoped_identifier`: `path::name` 形式。`path` は field=path、`name` は field=name
/// - `scoped_use_list`: `path::{...}` 形式。`path` は field=path、`list` は field=list (use_list)
/// - `use_list`: `{...}` 形式 (path なし、トップでは稀)
/// - `use_as_clause`: `path as alias` 形式。`path` は field=path、`alias` は field=alias
/// - `use_wildcard`: `path::*` 形式。`path` は field=path (省略あり)
/// - `crate` / `self` / `super`: アンカーキーワード (再帰中に処理)
///
/// 戻り値 `None` で「判定不能」(root を超える super::、解決不能な anchor) — 呼出元は index を `None` にする。
fn expand_rust_use_tree_edges_ast(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
    source_module: &[String],
    alias_override: Option<&str>,
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    match node.kind() {
        "scoped_use_list" => {
            let mut path_node: Option<tree_sitter::Node<'_>> = None;
            let mut list_node: Option<tree_sitter::Node<'_>> = None;
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "use_list" => {
                        list_node = Some(child);
                        break;
                    }
                    _ => path_node = Some(child),
                }
            }
            let list = list_node?;
            let resolved_prefix = match path_node {
                Some(pn) => {
                    let (prefix, leaf) =
                        resolve_use_path_node(pn, source, path_prefix, current_module)?;
                    let mut p = prefix;
                    if let Some(name) = leaf {
                        p.push(name);
                    }
                    p
                }
                None => path_prefix.to_vec(),
            };
            expand_use_list_edges(list, source, &resolved_prefix, source_module, out)?;
        }
        "use_list" => {
            expand_use_list_edges(node, source, path_prefix, source_module, out)?;
        }
        "use_as_clause" => {
            // [path, alias] 順。alias=`_` は外部非公開なので edge を作らない。
            let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                named.push(child);
            }
            if named.len() != 2 {
                return Some(());
            }
            let alias_text = named[1].utf8_text(source).ok()?.trim();
            if alias_text == "_" {
                return Some(());
            }
            let path_node = named[0];
            // use_as_clause の内側 path には alias がさらに適用されるケースは無いので alias_override
            // をここで指定して下流の `scoped_identifier` 経路で edge 化する。
            expand_rust_use_tree_edges_ast(
                path_node,
                source,
                path_prefix,
                current_module,
                source_module,
                Some(alias_text),
                out,
            )?;
        }
        "use_wildcard" => {
            // named child = [path]
            let mut cursor = node.walk();
            let path_node = node.named_children(&mut cursor).next();
            if let Some(path_node) = path_node {
                let (resolved_prefix, leaf_name) =
                    resolve_use_path_node(path_node, source, path_prefix, current_module)?;
                let mut target_module = resolved_prefix;
                if let Some(name) = leaf_name {
                    target_module.push(name);
                }
                if !target_module.is_empty() {
                    out.push(RustPubUseEdge::Wildcard {
                        source_module: source_module.to_vec(),
                        target_module,
                    });
                }
            }
        }
        "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
            // path::name 形式の単純 re-export、または anchor 単体。
            let (resolved_prefix, leaf_name) =
                resolve_use_path_node(node, source, path_prefix, current_module)?;
            if let Some(item) = leaf_name
                && !resolved_prefix.is_empty()
            {
                let exported_name = alias_override
                    .map(str::to_string)
                    .unwrap_or_else(|| item.clone());
                out.push(RustPubUseEdge::Named {
                    source_module: source_module.to_vec(),
                    exported_name,
                    target_module: resolved_prefix,
                    target_item: item,
                });
            }
        }
        _ => {
            // 知らない kind は子供を再帰 walk (将来の grammar 変更に保守的に対応)。
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    expand_rust_use_tree_edges_ast(
                        child,
                        source,
                        path_prefix,
                        current_module,
                        source_module,
                        alias_override,
                        out,
                    )?;
                }
            }
        }
    }
    Some(())
}

/// `use_list` ノード (`{ ... }`) の各要素 (`,` 区切り) を再帰展開して edge を出力する。
/// group 内では `current_module` は継承しない (空)。
fn expand_use_list_edges(
    list: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    source_module: &[String],
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        expand_rust_use_tree_edges_ast(child, source, path_prefix, &[], source_module, None, out)?;
    }
    Some(())
}

/// `path` ノード (scoped_identifier / identifier / crate / self / super) を解決して
/// `(resolved_prefix, leaf_name)` を返す。anchor (crate/self/super) を current_module で解決し、
/// scoped_identifier は再帰的に path → name を展開する。
///
/// 戻り値:
/// - `Some((prefix, Some(name)))`: scoped_identifier の終端で name 部分を抽出した
/// - `Some((prefix, None))`: anchor 単体 (super::, crate:: 等) のみで終わった
/// - `None`: 判定不能 (root を超える super::、不正な構造)
fn resolve_use_path_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
) -> Option<(Vec<String>, Option<String>)> {
    match node.kind() {
        "scoped_identifier" => {
            // tree-sitter-rust grammar は scoped_identifier で path/name の field 名を出さない。
            // named children は最大 2 つ: [path, name] または [name] のみ (path が省略された
            // 場合は crate root レベルの単一 identifier 扱い)。
            let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                named.push(child);
            }
            match named.as_slice() {
                [name_node] => {
                    let name_text = name_node.utf8_text(source).ok()?.trim().to_string();
                    Some((path_prefix.to_vec(), Some(name_text)))
                }
                [path_node, name_node] => {
                    let name_text = name_node.utf8_text(source).ok()?.trim().to_string();
                    let (prefix, intermediate_leaf) =
                        resolve_use_path_node(*path_node, source, path_prefix, current_module)?;
                    let mut full_prefix = prefix;
                    if let Some(leaf) = intermediate_leaf {
                        full_prefix.push(leaf);
                    }
                    Some((full_prefix, Some(name_text)))
                }
                _ => None, // 想定外の named children 数
            }
        }
        "identifier" => {
            let text = node.utf8_text(source).ok()?.trim().to_string();
            Some((path_prefix.to_vec(), Some(text)))
        }
        "crate" => {
            // crate root 起点: 現 prefix を捨ててルート (空) 起点にする。
            Some((Vec::new(), None))
        }
        "self" => {
            // 現 module 起点。current_module を prefix に積む (まだ何も積んでいない時のみ)。
            if path_prefix.is_empty() {
                Some((current_module.to_vec(), None))
            } else {
                Some((path_prefix.to_vec(), None))
            }
        }
        "super" => {
            // 現 module から 1 階層上。
            let mut effective = if path_prefix.is_empty() {
                current_module.to_vec()
            } else {
                path_prefix.to_vec()
            };
            effective.pop()?;
            Some((effective, None))
        }
        _ => {
            // 知らない kind は子供を再帰 walk して可能な解決を試みる。
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named()
                    && let Some(result) =
                        resolve_use_path_node(child, source, path_prefix, current_module)
                {
                    return Some(result);
                }
            }
            None
        }
    }
}

/// Cargo.toml のテキストから `[lib]` セクションが宣言されているかを判定する。
///
/// パース失敗時は **保守的に true (= library 宣言ありとみなす)** を返す。`api.rm` 側で
/// false negative (公開 API 削除の見逃し) を起こさない方向に倒すための既定値。
fn cargo_toml_text_declares_lib(text: &str) -> bool {
    match toml::from_str::<toml::Table>(text) {
        Ok(parsed) => parsed.contains_key("lib"),
        Err(_) => true,
    }
}

/// `name` が `file_path` 以外のファイルから参照されているかを判定する。
/// 参照検索に失敗した場合は保守的に true（＝外部参照ありとみなす）を返し、
/// modified の除外を抑止する（false positive を恐れて false negative を起こさない方針）。
///
/// `file_path` は `dir` からの相対パスを想定する。`find_references` の出力も
/// `dir` 相対なので `Path` 単位で比較する。
fn has_cross_file_refs(dir: &str, file_path: &str, name: &str) -> bool {
    use std::path::Path;

    let service = AppService::new();
    let Ok(refs_result) = service.find_references(name, dir, None) else {
        return true;
    };
    let self_path = Path::new(file_path);
    refs_result
        .references
        .iter()
        .any(|r| Path::new(r.path.as_str()) != self_path)
}

/// 削除されたシンボル `name` が、変更後のツリー全体のどこからも参照されていないかを判定する。
/// 参照が 0 件であれば同一 diff 内で全 caller が追随済みと判断し、`api.rm` から除外する。
/// 参照検索に失敗した場合は保守的に false（外部参照ありとみなす）を返し、
/// レビュー対象として残す（false negative を起こさない方針）。
/// `removed` 候補のうち、HEAD ツリーで repo 内参照 0 件のものを `removed_dead` に
/// 振り分ける。残りは `removed` (破壊的削除) として返す。
///
/// 実装上の配慮:
/// - **qualname → bare name**: `Container.method` 形式は refs 検索の identifier
///   マッチでは常に 0 件になるため、`bare_name` で正規化して検索する
/// - **batch refs**: 候補ごとに `find_references` を呼ぶと「候補数 × リポ全体走査」と
///   なる。`find_references_batch` で 1 回の AC + ディレクトリ走査に集約
/// - **同名複数定義の保守扱い**: 削除後の HEAD で同名 def が 2 件以上残っていれば
///   「部分削除」「同名複数 export」など破壊的削除の可能性があるため、保守的に
///   `removed` に残す (false negative より false positive を優先)
/// - **検索失敗時の保守扱い**: batch refs が `Err` を返した場合、すべて `removed`
///   に残す (false negative 防止)
fn partition_removed_dead_candidates(
    dir: &str,
    candidates: Vec<ApiSymbolCandidate>,
) -> (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) {
    use crate::models::reference::RefKind;
    use std::collections::{HashMap, HashSet};

    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // 候補から bare name を重複排除して集める
    let mut unique_bare: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in &candidates {
        let bare = bare_name(&c.name).to_string();
        if seen.insert(bare.clone()) {
            unique_bare.push(bare);
        }
    }

    let service = AppService::new();
    let batch_result = match service.find_references_batch(&unique_bare, dir, None) {
        Ok(r) => r,
        Err(_) => {
            // 検索失敗時は保守的にすべて removed に残す
            return (candidates, Vec::new());
        }
    };

    // 外部パッケージ (package.json deps) から import された同名 binding は、削除した
    // ローカルシンボルとは別物 (例: tailwindcss の `Config` 型) なので参照カウントから
    // 除外する。これがないと汎用名の削除が外部同名 import を拾って api.rm に誤分類される
    // (codex 設計合意。full TS resolver は入れず、証明できる外部 import binding のみ除外)。
    let external_pkgs = load_external_package_names(dir);
    // (path, symbol) -> (外部 import の local binding が symbol か, 外部 import 元名が symbol の行集合)
    let mut import_info_cache: HashMap<(String, String), (bool, HashSet<usize>)> = HashMap::new();

    // bare_name -> (def_count, ref_count)
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for r in &batch_result {
        let mut def_count = 0usize;
        let mut ref_count = 0usize;
        for x in &r.references {
            if x.kind == Some(RefKind::Definition) {
                def_count += 1;
                continue;
            }
            let key = (x.path.clone(), r.symbol.clone());
            let (local_bound, source_name_lines) =
                import_info_cache.entry(key).or_insert_with(|| {
                    analyze_external_import_for_symbol(dir, &x.path, &r.symbol, &external_pkgs)
                });
            // 外部 import specifier の import 元名そのものの参照 (import 行) は別モジュールの
            // export 名なので数えない (`import { Config as X } from "pkg"` の `Config`)。
            if source_name_lines.contains(&x.line) {
                continue;
            }
            // 外部 import の local binding を持つファイルなら、その使用箇所も外部由来として
            // 数えない (`import { Config } from "pkg"` の local Config 利用)。
            if *local_bound {
                continue;
            }
            ref_count += 1;
        }
        counts.insert(r.symbol.clone(), (def_count, ref_count));
    }

    let mut removed_kept = Vec::new();
    let mut removed_dead = Vec::new();
    for c in candidates {
        let bare = bare_name(&c.name).to_string();
        let (def_count, ref_count) = counts.get(&bare).copied().unwrap_or((0, 0));
        // 同名定義が複数残っている → 保守的に removed に残す
        if def_count > 1 {
            removed_kept.push(c);
            continue;
        }
        if ref_count == 0 {
            removed_dead.push(c);
        } else {
            removed_kept.push(c);
        }
    }

    (removed_kept, removed_dead)
}

/// `<dir>/package.json` の dependencies / devDependencies / peerDependencies /
/// optionalDependencies のキー (外部パッケージ名) を集める。package.json 不在 / パース
/// 失敗時は空集合 (= 何も除外しない、保守的)。
fn load_external_package_names(dir: &str) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let path = std::path::Path::new(dir).join("package.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return HashSet::new();
    };
    let mut pkgs = HashSet::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = json.get(key).and_then(|v| v.as_object()) {
            for name in obj.keys() {
                pkgs.insert(name.clone());
            }
        }
    }
    pkgs
}

/// import specifier から npm パッケージ名を取り出す。相対 (`./` `../` `/`) / alias
/// (`@/` `~/` `#`) は外部パッケージではないため None (保守的に内部扱い)。scoped は
/// `@scope/pkg`、bare は最初のセグメント。
fn import_specifier_package_name(spec: &str) -> Option<String> {
    if spec.is_empty()
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || spec.starts_with("@/")
        || spec.starts_with("~/")
        || spec.starts_with('#')
    {
        return None;
    }
    if let Some(scoped) = spec.strip_prefix('@') {
        // @scope/pkg[/sub]
        let mut parts = scoped.splitn(3, '/');
        let scope = parts.next()?;
        let pkg = parts.next()?;
        if scope.is_empty() || pkg.is_empty() {
            return None;
        }
        return Some(format!("@{scope}/{pkg}"));
    }
    spec.split('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `ref_path` の TS/JS ファイル内で、`symbol` が外部パッケージ (`external_pkgs`) からの
/// import で束縛されているか。束縛されていれば、そのファイルの `symbol` 参照は削除した
/// ローカルシンボルとは別物 (別モジュールの同名型) と判断できる。
/// 非 TS/JS / 読み込み・parse 失敗 / external_pkgs 空は false (除外しない、保守的)。
/// `ref_path` の TS/JS ファイルを解析し、`symbol` に関する外部パッケージ import 情報を返す。
/// 戻り値 `(local_bound, source_name_lines)`:
/// - `local_bound`: 外部パッケージ (`external_pkgs`) からの import で local binding が `symbol`
///   (`import { Config }` / `import { Foo as Config }` / `import Config` / `import * as Config`)。
///   この場合ファイル内の `symbol` 利用は外部由来 (使用箇所も除外対象)。
/// - `source_name_lines`: 外部 import specifier の **import 元名** が `symbol` の行 (0-indexed)。
///   `import { Config as X } from "pkg"` の `Config` は別モジュールの export 名なので、その
///   import 行の参照だけを除外する (使用箇所は local binding X で別物)。
///
/// 非 TS/JS / 読み込み・parse 失敗 / external_pkgs 空は `(false, 空集合)` (除外しない、保守的)。
fn analyze_external_import_for_symbol(
    dir: &str,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &std::collections::HashSet<String>,
) -> (bool, std::collections::HashSet<usize>) {
    use crate::language::LangId;
    use std::collections::HashSet;
    let empty = (false, HashSet::new());
    if external_pkgs.is_empty() {
        return empty;
    }
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return empty;
    };
    let Ok(lang) = LangId::from_path(utf8) else {
        return empty;
    };
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return empty;
    }
    let Ok(source) = parser::read_file(utf8) else {
        return empty;
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return empty;
    };
    let root = tree.root_node();
    let mut local_bound = false;
    let mut source_name_lines = HashSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_statement" {
            continue;
        }
        let Some(src_node) = child.child_by_field_name("source") else {
            continue;
        };
        let Some(spec) = static_js_string_text(src_node, &source) else {
            continue;
        };
        let Some(pkg) = import_specifier_package_name(spec) else {
            continue;
        };
        if !external_pkgs.contains(&pkg) {
            continue;
        }
        collect_external_import_bindings(
            child,
            &source,
            symbol,
            &mut local_bound,
            &mut source_name_lines,
        );
    }
    (local_bound, source_name_lines)
}

/// 外部パッケージ import 文 `import_stmt` を解析し、`symbol` の local binding 有無を
/// `local_bound` に、import 元名が `symbol` の出現行を `source_name_lines` に記録する。
fn collect_external_import_bindings(
    import_stmt: tree_sitter::Node,
    source: &[u8],
    symbol: &str,
    local_bound: &mut bool,
    source_name_lines: &mut std::collections::HashSet<usize>,
) {
    let mut cursor = import_stmt.walk();
    let Some(clause) = import_stmt
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause")
    else {
        return;
    };
    let mut clause_cursor = clause.walk();
    for child in clause.named_children(&mut clause_cursor) {
        match child.kind() {
            // default import: `import Config from "..."`
            "identifier" => {
                if child.utf8_text(source).ok() == Some(symbol) {
                    *local_bound = true;
                }
            }
            // namespace import: `import * as Config from "..."`
            "namespace_import" => {
                let mut ns = child.walk();
                if child
                    .named_children(&mut ns)
                    .any(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(symbol))
                {
                    *local_bound = true;
                }
            }
            // named imports: `import { Foo, Bar as Baz } from "..."`
            "named_imports" => {
                let mut ni = child.walk();
                for spec in child.named_children(&mut ni) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let name_node = spec.child_by_field_name("name");
                    // import 元名が symbol → その出現行を記録 (別モジュールの export 名)
                    if let Some(name) = name_node
                        && name.utf8_text(source).ok() == Some(symbol)
                    {
                        source_name_lines.insert(name.start_position().row);
                    }
                    // local binding (alias があれば alias、無ければ name) が symbol → 利用も外部
                    let local = spec.child_by_field_name("alias").or(name_node);
                    if local.and_then(|n| n.utf8_text(source).ok()) == Some(symbol) {
                        *local_bound = true;
                    }
                }
            }
            _ => {}
        }
    }
}

/// 削除されたシンボル `name` が、変更後のツリー全体のどこからも参照されていないかを判定する。
/// 参照が 0 件であれば同一 diff 内で全 caller が追随済みと判断し、`api.rm` から除外する。
/// 参照検索に失敗した場合は保守的に false（外部参照ありとみなす）を返し、
/// レビュー対象として残す（false negative を起こさない方針）。
///
/// qualname (`Container.method`) は refs 検索の identifier マッチでは常に 0 件になるため、
/// `bare_name` で正規化して検索する。同名定義が HEAD ツリーに 2 件以上残存する場合は
/// 「部分削除」「同名複数 export」の可能性があるため保守的に false を返す
/// (codex 指摘: detect_api_changes の早期 continue 経路でも qualname 対応が必要)。
fn is_removed_symbol_unreferenced(dir: &str, name: &str) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(bare, dir, None) else {
        return false;
    };
    let mut def_count = 0usize;
    let mut ref_count = 0usize;
    for r in &refs_result.references {
        if r.kind == Some(RefKind::Definition) {
            def_count += 1;
        } else {
            ref_count += 1;
        }
    }
    if def_count > 1 {
        return false;
    }
    ref_count == 0
}

/// 削除された bash 関数 `name` が、変更後ツリーの bash 系ファイル内のどこからも
/// 参照されていないかを判定する。CLI スクリプトを別言語に書き換えたときに、
/// 新言語側の同名定義/参照を「別物」として扱うため bash ファイル限定で検索する。
/// 参照検索に失敗した場合は保守的に false を返してレビュー対象として残す。
fn is_removed_bash_symbol_unreferenced(dir: &str, name: &str) -> bool {
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(name, dir, None) else {
        return false;
    };
    refs_result
        .references
        .iter()
        .all(|r| !is_bash_script_path(r.path.as_str()))
}

/// 拡張子から bash 系シェルスクリプトファイル（.sh / .bash / .zsh）かを判定する。
fn is_bash_script_path(file_path: &str) -> bool {
    std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext, "sh" | "bash" | "zsh"))
}

/// `git show <base>:<file_path>` の内容から bash 関数 `name` が `export -f` 等で
/// 明示的にエクスポートされているか判定する。base 側の取得に失敗した場合は
/// 保守的に false（未 export 扱い）を返す。
fn bash_function_is_exported_in_git(dir: &str, base: &str, file_path: &str, name: &str) -> bool {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file_path, "diff file path").is_err()
    {
        return false;
    }
    let Ok(output) = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return false;
    };
    bash_has_export_f(text, name)
}

/// shell ソース文字列に `export -f <name>` / `declare -fx <name>` / `declare -xf <name>`
/// による関数エクスポート宣言が含まれているかを判定する。
///
/// 各行を `trim_start()` してから先頭一致を見るため、インデント付きの宣言にも対応する。
/// 同一行に複数名を列挙する形式 (`export -f foo bar`) もサポートする。
fn bash_has_export_f(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    const PREFIXES: &[&str] = &["export -f ", "declare -fx ", "declare -xf "];
    for line in source.lines() {
        let trimmed = line.trim_start();
        for prefix in PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                for token in rest.split_whitespace() {
                    if token == name {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Python のクラス内に存在するフィールド宣言 (`name: type` 形式) を集める。
///
/// `@property def x(self) -> T` から `@dataclass` フィールド `x: T` への置き換えを検出する
/// ために使う。tree-sitter で `class_definition` を走査し、`name` フィールドが `class_name`
/// と一致するクラスの body 直下にある `name: type` 宣言の左辺 identifier を返す。
fn extract_python_class_fields(
    dir: &str,
    file_path: &str,
    class_name: &str,
) -> std::collections::HashSet<String> {
    let mut fields = std::collections::HashSet::new();
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = match camino::Utf8Path::from_path(&full_path) {
        Some(p) => p,
        None => return fields,
    };
    let lang_id = match crate::language::LangId::from_path(utf8_path) {
        Ok(l) => l,
        Err(_) => return fields,
    };
    if lang_id != crate::language::LangId::Python {
        return fields;
    }
    let source = match parser::read_file(utf8_path) {
        Ok(s) => s,
        Err(_) => return fields,
    };
    let tree = match parser::parse_source(&source, lang_id) {
        Ok(t) => t,
        Err(_) => return fields,
    };

    walk_python_class_for_fields(tree.root_node(), &source, class_name, &mut fields);
    fields
}

fn walk_python_class_for_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    class_name: &str,
    out: &mut std::collections::HashSet<String>,
) {
    if node.kind() == "class_definition"
        && let Some(name_node) = node.child_by_field_name("name")
        && name_node.utf8_text(source).ok() == Some(class_name)
        && let Some(body) = node.child_by_field_name("body")
    {
        collect_python_dataclass_fields(body, source, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_python_class_for_fields(child, source, class_name, out);
    }
}

/// Python のクラス body 直下にある `name: type` 形式の宣言の左辺 identifier を集める。
///
/// tree-sitter-python では `name: type` (右辺なし) は `expression_statement > assignment`
/// に展開され、`assignment.left = identifier` / `assignment.type` が存在する。`name: type = default`
/// の形式も同じく `assignment` ノードで `right` が追加されるだけなので同じハンドラで取れる。
fn collect_python_dataclass_fields(
    body: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut std::collections::HashSet<String>,
) {
    let mut cursor = body.walk();
    for stmt in body.children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let mut sub_cursor = stmt.walk();
        for sub in stmt.children(&mut sub_cursor) {
            if sub.kind() != "assignment" {
                continue;
            }
            let Some(left) = sub.child_by_field_name("left") else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            // `type` フィールドが存在するもの（typed annotation）のみ対象
            if sub.child_by_field_name("type").is_none() {
                continue;
            }
            if let Ok(name) = left.utf8_text(source) {
                out.insert(name.to_string());
            }
        }
    }
}

/// Python の `@property def member(self) -> T` を `@dataclass` フィールド `member: T` に
/// 置き換えた変更を検出する。
///
/// `qualname` は `Container.member` 形式の文字列。`diff_new_paths` 内のいずれかの新ファイルに
/// 同名 `Container` クラスが存在し、その中に `member: type` の typed annotation 宣言が
/// あれば、それが置き換え先のファイルパスであるとして返す。複数候補があれば最初のものを返す。
fn detect_python_property_to_field(
    dir: &str,
    qualname: &str,
    diff_new_paths: &HashSet<String>,
) -> Option<String> {
    let (container, member) = qualname.split_once('.')?;
    if container.is_empty() || member.is_empty() {
        return None;
    }
    // qualname がさらにネストしている場合 (`A.B.member`) は保守的に対象外とする。
    if member.contains('.') {
        return None;
    }
    for new_path in diff_new_paths {
        if !std::path::Path::new(new_path)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("py"))
            .unwrap_or(false)
        {
            continue;
        }
        let fields = extract_python_class_fields(dir, new_path, container);
        if fields.contains(member) {
            return Some(new_path.clone());
        }
    }
    None
}

mod batch;
mod session_handler;

pub use batch::{batch_ast, batch_calls, batch_imports, batch_lint, batch_sequence, batch_symbols};
pub use session_handler::handle_request;

#[cfg(test)]
mod tests;
