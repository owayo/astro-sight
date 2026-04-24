use anyhow::{Result, anyhow, bail};
use rayon::prelude::*;
use std::collections::HashSet;
use std::io::Read;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::CoChangeOptions;
use crate::models::dead_code::DeadCodeResult;
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, DeadSymbol, MissingCochange, ReviewResult,
};
use crate::service::{AppService, AstParams};

// ---------------------------------------------------------------------------
// 共通ヘルパー
// ---------------------------------------------------------------------------

const MAX_INPUT_SIZE: usize = 100 * 1024 * 1024;

pub fn classify_error(e: &anyhow::Error) -> (String, String) {
    if let Some(ae) = e.downcast_ref::<AstroError>() {
        (ae.code.to_string(), ae.message.clone())
    } else {
        ("IO_ERROR".to_string(), e.to_string())
    }
}

pub fn serialize_output(value: &impl serde::Serialize, pretty: bool) -> Result<String> {
    if pretty {
        Ok(serde_json::to_string_pretty(value)?)
    } else {
        Ok(serde_json::to_string(value)?)
    }
}

fn make_error_line(e: &anyhow::Error) -> String {
    let (code, message) = classify_error(e);
    let obj = serde_json::json!({ "error": { "code": code, "message": message } });
    serde_json::to_string(&obj).unwrap()
}

fn read_bytes_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut limited = reader.take((max_bytes + 1) as u64);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf)?;

    if buf.len() > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                buf.len(),
                max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

fn read_bytes_limited_and_drain<R: std::io::Read>(
    mut reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut total_bytes = 0usize;
    let mut chunk = [0u8; 8192];

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }

        total_bytes = total_bytes.saturating_add(read);
        if buf.len() <= max_bytes {
            let remaining = max_bytes.saturating_add(1).saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    }

    if total_bytes > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                total_bytes, max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

fn read_to_string_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<String> {
    let buf = read_bytes_limited(reader, max_bytes, source_name)?;
    String::from_utf8(buf).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{source_name} is not valid UTF-8: {e}"),
        )
        .into()
    })
}

fn read_file_to_string_limited(path: &str, max_bytes: usize) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes as u64 {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{path} exceeds maximum size ({} bytes > {} bytes)",
                metadata.len(),
                max_bytes
            ),
        )
        .into());
    }
    read_to_string_limited(file, max_bytes, path)
}

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
    let hash = CacheStore::hash(&source);
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
    let hash = CacheStore::hash(&source);
    let use_cache = !no_cache && !pretty;

    let cache_key = if full {
        "symbols_full"
    } else if doc {
        "v2_symbols_doc"
    } else {
        "v2_symbols"
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
    let results = service.find_references_batch(names, dir, glob)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for result in &results {
        let line = serde_json::to_string(result)?;
        writeln!(out, "{line}")?;
    }
    info!(
        command = "refs_batch",
        names_count = names.len(),
        total_refs = results.iter().map(|r| r.references.len()).sum::<usize>(),
        "command completed"
    );
    Ok(())
}

pub fn cmd_cochange(
    service: &AppService,
    dir: &str,
    opts: &CoChangeOptions,
    pretty: bool,
) -> Result<()> {
    let result = service.analyze_cochange(dir, opts)?;
    let output = serialize_output(&result, pretty)?;
    info!(
        command = "cochange",
        dir = dir,
        lookback = opts.lookback,
        min_confidence = opts.min_confidence,
        min_samples = opts.min_samples,
        max_files_per_commit = opts.max_files_per_commit,
        bounded_by_merge_base = opts.bounded_by_merge_base,
        skip_deleted_files = opts.skip_deleted_files,
        file = ?opts.filter_file,
        output_bytes = output.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

/// `git diff` / `git show` に渡す revision を検証する。
/// 先頭が `-` の値は git がオプションとして解釈するため拒否する。
/// (例: `--output=/path` によるファイル書き込みを防ぐ)
fn validate_git_revision(rev: &str, arg_name: &str) -> Result<()> {
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
    // `\0` を含む値はプロセス引数として不正
    if rev.contains('\0') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not contain NUL"),
        ));
    }
    Ok(())
}

pub fn run_git_diff(dir: &str, base: &str, staged: bool) -> Result<String> {
    validate_git_revision(base, "--base")?;
    // git config `diff.renames` がユーザー環境で無効化されていても rename を検出できるよう、
    // 明示的に `--find-renames` を指定する。ファイル rename が api.rm/api.add の誤発報源に
    // なるため、astro-sight 側で強制しておく。
    let mut args = vec!["diff".to_string(), "--find-renames".to_string()];
    if staged {
        args.push("--cached".to_string());
    }
    args.push(base.to_string());

    let mut child = std::process::Command::new("git")
        .args(&args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture git diff stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture git diff stderr"))?;

    let stdout_handle = std::thread::spawn(move || {
        // 子プロセスのパイプは上限超過後も読み捨てて、wait() の詰まりを防ぐ。
        read_bytes_limited_and_drain(stdout, MAX_INPUT_SIZE, "git diff output")
    });
    let stderr_handle = std::thread::spawn(move || {
        read_bytes_limited_and_drain(stderr, MAX_INPUT_SIZE, "git diff stderr")
    });

    let status = child.wait()?;
    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| anyhow!("Failed to join git diff stdout reader"))??;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| anyhow!("Failed to join git diff stderr reader"))??;

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("git diff failed: {stderr}"),
        )
        .into());
    }

    String::from_utf8(stdout_bytes).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("git diff output is not valid UTF-8: {e}"),
        )
        .into()
    })
}

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
) -> Result<()> {
    let diff_input = if let Some(d) = diff {
        d.to_string()
    } else if let Some(df) = diff_file {
        read_file_to_string_limited(df, MAX_INPUT_SIZE)?
    } else if git {
        run_git_diff(dir, base, staged)?
    } else {
        let stdin = std::io::stdin();
        read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
    };

    if pretty {
        // pretty 出力は人間向けで整形が必要なため、従来どおり全 FileImpact を集約してから
        // 一括 serialize する。数 GB 級リポでは compact 出力推奨。
        let result = service.analyze_context(&diff_input, dir)?;
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
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(b"{\"changes\":[")?;
    let mut first = true;
    let mut changes_count = 0usize;
    service.analyze_context_streaming(&diff_input, dir, |impact| {
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

pub fn cmd_impact(
    service: &AppService,
    dir: &str,
    git: bool,
    base: &str,
    staged: bool,
    hook: bool,
) -> Result<()> {
    let diff_input = if git {
        run_git_diff(dir, base, staged)?
    } else {
        let stdin = std::io::stdin();
        read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?
    };

    if diff_input.trim().is_empty() {
        return Ok(());
    }

    let result = service.analyze_context(&diff_input, dir)?;

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
) -> Result<()> {
    // 1. diff 取得（context コマンドと同じ入力方式）
    let diff_input = if let Some(d) = diff {
        d.to_string()
    } else if let Some(df) = diff_file {
        read_file_to_string_limited(df, MAX_INPUT_SIZE)?
    } else if git {
        run_git_diff(dir, base, staged)?
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
    let impact = service.analyze_context(&diff_input, dir)?;

    // 3. diff に含まれるファイルリストを収集
    let diff_files = crate::engine::diff::parse_unified_diff(&diff_input);
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
    let missing_cochanges =
        detect_missing_cochanges(service, dir, &changed_file_set, min_confidence);

    // 5. API surface diff
    let api_changes = detect_api_changes(dir, base, &diff_files);

    // 6. dead symbol 検出
    let dead_symbols = detect_dead_symbols(dir, &diff_files);

    let result = ReviewResult {
        impact,
        missing_cochanges,
        api_changes,
        dead_symbols,
    };

    if hook {
        return review_hook_output(&result, dir);
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
    ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
    }
}

/// `--hook` の出力判定結果。
/// - `value`: stderr に書き出す JSON (何もなければ None)
/// - `is_blocking`: exit 1 にして Stop hook を止めるべきか。cochange だけは informational
///   として block しない (レポート 2026-04-11-cochange-new-repo-initial-commit-noise.md の提案)
struct HookJsonBuild {
    value: Option<serde_json::Value>,
    is_blocking: bool,
}

fn build_review_hook_json(result: &ReviewResult, dir: &str) -> HookJsonBuild {
    #[derive(Default)]
    struct HookImpactGroup {
        changed_symbols: std::collections::BTreeSet<String>,
        refs: Vec<(String, usize, Vec<String>)>,
    }

    // 未解決 impact を収集
    let changed_paths: std::collections::HashSet<&str> = result
        .impact
        .changes
        .iter()
        .map(|c| c.path.as_str())
        .collect();
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

    let mut unresolved: std::collections::BTreeMap<String, HookImpactGroup> =
        std::collections::BTreeMap::new();
    for change in &result.impact.changes {
        if change.affected_symbols.is_empty() {
            continue;
        }
        for caller in &change.impacted_callers {
            let caller_abs = if std::path::Path::new(&caller.path).is_relative() {
                std::path::Path::new(dir)
                    .join(&caller.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                caller.path.clone()
            };
            let in_diff = match std::fs::canonicalize(&caller_abs) {
                Ok(canon) => changed_canonical.contains(&canon),
                Err(_) => changed_abs_strs.contains(&caller_abs),
            };
            if !in_diff {
                let entry = unresolved.entry(change.path.clone()).or_default();
                entry.changed_symbols.extend(
                    change
                        .affected_symbols
                        .iter()
                        .map(|symbol| symbol.name.clone()),
                );
                entry
                    .refs
                    .push((caller.path.clone(), caller.line, caller.symbols.clone()));
            }
        }
    }

    // 空セクションは省略した compact JSON を構築
    let mut hook_obj = serde_json::Map::new();
    // has_blocking_issues: Stop hook を止めるべき重要な検出 (impacts / api / dead)
    // has_any_output: 出力すべき検出 (上記 + cochange)
    let mut has_blocking_issues = false;
    let mut has_any_output = false;

    // impacts: [{src,syms,refs:[{p,ln,s}]}]
    if !unresolved.is_empty() {
        has_blocking_issues = true;
        has_any_output = true;
        let impacts: Vec<serde_json::Value> = unresolved
            .iter()
            .map(|(changed_path, group)| {
                let refs: Vec<serde_json::Value> = group
                    .refs
                    .iter()
                    .map(|(p, ln, s)| {
                        let mut r = serde_json::Map::new();
                        r.insert("p".into(), serde_json::Value::String(p.clone()));
                        r.insert("ln".into(), serde_json::json!(*ln));
                        if !s.is_empty() {
                            r.insert(
                                "s".into(),
                                serde_json::Value::Array(
                                    s.iter()
                                        .map(|v| serde_json::Value::String(v.clone()))
                                        .collect(),
                                ),
                            );
                        }
                        serde_json::Value::Object(r)
                    })
                    .collect();
                serde_json::json!({
                    "src": changed_path,
                    "syms": group.changed_symbols.iter().collect::<Vec<_>>(),
                    "refs": refs,
                })
            })
            .collect();
        hook_obj.insert("impacts".into(), serde_json::Value::Array(impacts));
    }

    // cochange: [{f,w,c}] — 情報提供のみ。is_blocking にはしない
    if !result.missing_cochanges.is_empty() {
        has_any_output = true;
        let cochanges: Vec<serde_json::Value> = result
            .missing_cochanges
            .iter()
            .map(|mc| {
                serde_json::json!({
                    "f": mc.file,
                    "w": mc.expected_with,
                    "c": (mc.confidence * 100.0).round() as u32,
                })
            })
            .collect();
        hook_obj.insert("cochange".into(), serde_json::Value::Array(cochanges));
    }

    // api: {add,rm,mod} — 空でないセクションのみ
    let has_api_changes = !result.api_changes.added.is_empty()
        || !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty();
    // api.added は新規 pub シンボル追加 (additive) で既存コードを壊さないため Stop hook の
    // ブロッキング対象から外し informational 扱いにする。api.removed / api.modified は
    // 破壊的変更の可能性があるため従来どおり blocking。
    let has_api_breaking =
        !result.api_changes.removed.is_empty() || !result.api_changes.modified.is_empty();
    if has_api_changes {
        if has_api_breaking {
            has_blocking_issues = true;
        }
        has_any_output = true;
        let mut api = serde_json::Map::new();
        if !result.api_changes.added.is_empty() {
            api.insert(
                "add".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .added
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.removed.is_empty() {
            api.insert(
                "rm".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .removed
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.modified.is_empty() {
            api.insert(
                "mod".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .modified
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        hook_obj.insert("api".into(), serde_json::Value::Object(api));
    }

    // dead: [{n,f}]
    if !result.dead_symbols.is_empty() {
        has_blocking_issues = true;
        has_any_output = true;
        let dead: Vec<serde_json::Value> = result
            .dead_symbols
            .iter()
            .map(|ds| serde_json::json!({"n": ds.name, "f": ds.file}))
            .collect();
        hook_obj.insert("dead".into(), serde_json::Value::Array(dead));
    }

    if !has_any_output {
        return HookJsonBuild {
            value: None,
            is_blocking: false,
        };
    }

    hook_obj.insert(
        "hint".into(),
        serde_json::Value::String("False positives? Run astro-sight-triage skill.".into()),
    );

    HookJsonBuild {
        value: Some(serde_json::Value::Object(hook_obj)),
        is_blocking: has_blocking_issues,
    }
}

/// --hook 時の review 出力: compact JSON を stderr に出力する。
/// blocking な検出 (impacts / api / dead) があれば exit 1、
/// cochange のみの informational な出力は exit 0 にして Stop hook を止めない。
fn review_hook_output(result: &ReviewResult, dir: &str) -> Result<()> {
    let build = build_review_hook_json(result, dir);
    let Some(hook_output) = build.value else {
        return Ok(());
    };

    eprintln!("{hook_output}");
    if build.is_blocking {
        std::process::exit(1);
    }
    Ok(())
}

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
) -> Vec<MissingCochange> {
    let opts = CoChangeOptions {
        min_confidence,
        ..CoChangeOptions::default()
    };
    let cochange_result = match service.analyze_cochange(dir, &opts) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
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
    missing
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
    let mut modified = Vec::new();

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
                // Rust binary crate (src/lib.rs が存在しない crate) の pub シンボルは
                // クレート外から到達できないため api.add の対象外とする。
                let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);
                // 新規ファイルでも、同一ファイル内で呼ばれている関数は内部ヘルパーと
                // 判断して api.add から除外する。CLI スクリプト (main から内部関数を
                // 呼び出す構造) を新規追加した時に全関数が api.add に積まれるノイズを防ぐ。
                let in_file_callees = extract_in_file_callees(dir, &df.new_path);
                for (name, kind, sig) in &new_syms {
                    if is_binary_rust_crate {
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
                    added.push(ApiSymbolCandidate {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: df.new_path.clone(),
                        signature: sig.clone(),
                    });
                }
            }
            continue;
        }

        if df.new_path == "/dev/null" {
            if let Some(old_syms) = extract_exported_symbols_from_git(dir, base, &df.old_path) {
                for (name, kind, sig) in &old_syms {
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

        // 新ファイル内の call 先名を集める。同一ファイル内から呼ばれている新規関数は
        // 「内部ヘルパー」として api.add から除外する（Bash スクリプトのトップレベル関数や
        // Python の同一ファイル内で接続済みの private 関数が api.add に出る誤検出対策）。
        let in_file_callees = extract_in_file_callees(dir, &df.new_path);

        // Rust binary crate (src/lib.rs が存在しない crate) の pub シンボルは
        // クレート外から到達できないため api.add の対象外とする。
        let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);

        // rename 検出用: 同ファイル内に新規追加された全シンボル名を追跡する
        // （internally_connected で除外される内部ヘルパーも含む）。削除シンボルと
        // 組み合わせて「rename + 実装置換」の api.rm ノイズを抑止する。
        let mut new_symbols_in_current_file: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for (name, kind, sig) in &new_syms {
            if !old_map.contains_key(name.as_str()) {
                new_symbols_in_current_file.insert(name.clone());
                if is_binary_rust_crate {
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
                added.push(ApiSymbolCandidate {
                    name: name.clone(),
                    kind: kind.clone(),
                    file: df.new_path.clone(),
                    signature: sig.clone(),
                });
            }
        }

        for (name, kind, sig) in &old_syms {
            if !new_map.contains_key(name.as_str()) {
                // closed-in-diff for api.rm: 同ファイルに新規追加されたシンボルがあり、
                // 削除されたシンボルが変更後ツリーで 0 件参照なら「rename + 実装置換」
                // と判断して api.rm から除外する。caller は同一 diff 内で追随済み。
                // 純粋な関数削除（新規追加がない）は api.rm に残す。
                if !new_symbols_in_current_file.is_empty()
                    && is_removed_symbol_unreferenced(dir, name)
                {
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

        // 同一 (file, qualname) の modified を重複排除するためのキーセット
        let mut seen_modified: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for (name, kind, new_sig) in &new_syms {
            if let Some(old_sig) = old_map.get(name.as_str())
                && old_sig != &new_sig.as_str()
                && seen_modified.insert((df.new_path.clone(), name.clone()))
            {
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

                modified.push(ApiSymbolChange {
                    name: name.clone(),
                    kind: kind.clone(),
                    file: df.new_path.clone(),
                    old_signature: Some(old_sig.to_string()),
                    new_signature: Some(new_sig.clone()),
                });
            }
        }
    }

    // git の rename detection が効かない diff (外部供給 / 非 git 入力 / 設定で無効化された
    // 環境など) に対するフォールバックとして、同一 (name, kind, signature) の add/rm ペアを
    // rename または move として相殺する。
    let (added, removed) = reconcile_api_symbols(added, removed);

    ApiChanges {
        added: added.into_iter().map(|c| c.into_api_symbol()).collect(),
        removed: removed.into_iter().map(|c| c.into_api_symbol()).collect(),
        modified,
    }
}

/// 同名・同種別・同シグネチャの api.add と api.rm のペアを相殺する。
/// 1 対 1 マッチングで相殺し、残ったものだけを返す。
fn reconcile_api_symbols(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
) -> (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) {
    use std::collections::HashMap;
    use std::collections::VecDeque;

    let mut removed_bucket: HashMap<(String, String, String), VecDeque<ApiSymbolCandidate>> =
        HashMap::new();
    for sym in removed {
        removed_bucket
            .entry((sym.name.clone(), sym.kind.clone(), sym.signature.clone()))
            .or_default()
            .push_back(sym);
    }

    let mut kept_added = Vec::with_capacity(added.len());
    for sym in added {
        let key = (sym.name.clone(), sym.kind.clone(), sym.signature.clone());
        if let Some(bucket) = removed_bucket.get_mut(&key)
            && bucket.pop_front().is_some()
        {
            // 同一ペアが rm 側にあれば相殺
            continue;
        }
        kept_added.push(sym);
    }

    let kept_removed: Vec<ApiSymbolCandidate> = removed_bucket
        .into_values()
        .flat_map(|bucket| bucket.into_iter())
        .collect();

    (kept_added, kept_removed)
}

fn detect_dead_symbols(
    dir: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> Vec<DeadSymbol> {
    let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
        return Vec::new();
    };
    let files: Vec<std::path::PathBuf> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        .map(|f| canonical_dir.join(&f.new_path))
        .collect();
    detect_dead_symbols_from_files(dir, &files, None)
}

/// qualname (`Container.method`) から末尾セグメントのみを抜き出す。
/// `a.b.c` → `c`、`foo` → `foo`。
fn bare_name(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// ファイルリストからエクスポートシンボルを収集し、参照ゼロのシンボルを返す。
/// dead-code コマンドと review コマンドの共通コアロジック。
/// count_non_definition_refs_batch で件数のみカウントし、SymbolReference を確保しない。
pub(crate) fn detect_dead_symbols_from_files(
    dir: &str,
    files: &[std::path::PathBuf],
    glob: Option<&str>,
) -> Vec<DeadSymbol> {
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    // .gitattributes の linguist-generated 指定ファイルは dead-code 検出から除外する
    let gitattrs = crate::engine::gitattributes::GitAttributes::load(&canonical_dir);

    // 全ファイルのエクスポートシンボルを収集（trait impl メソッドは除外）
    // (original_name, kind, file, lang_id) — case-insensitive 言語では lang_id で
    // シンボル名を正規化した比較を行うため lang も保持する。
    let mut all_syms: Vec<(String, String, String, crate::language::LangId)> = Vec::new();
    for path in files {
        // canonicalize で削除済みファイルをスキップ、dir 外のパスも除外
        let canonical_path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel = match canonical_path.strip_prefix(&canonical_dir) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue, // dir 外のパスは除外（セキュリティ境界）
        };
        if gitattrs.is_generated(&rel) {
            continue;
        }
        // ファイル先頭の「自動生成」マーカーコメントでも除外する (.gitattributes が
        // 無いリポジトリでも tree-sitter の parser.c / protoc の *.pb.go 等を無視できる)
        if crate::engine::generated::is_auto_generated(&canonical_path) {
            continue;
        }
        let lang = match crate::language::LangId::from_path(camino::Utf8Path::new(&rel)) {
            Ok(l) => l,
            Err(_) => continue,
        };
        if let Some(syms) = extract_dead_code_candidates_from_file(dir, &rel) {
            for (name, kind, _sig) in syms {
                all_syms.push((name, kind, rel.clone(), lang));
            }
        }
    }

    if all_syms.is_empty() {
        return Vec::new();
    }

    // refs 検索は AST 上の identifier ノードに対してマッチするため、
    // `Container.method` 形式の qualname ではマッチせず常に 0 件となってしまう。
    // そのため検索キーは末尾セグメント（bare name）に統一する。
    // 同名シンボルが複数箇所にある場合は保守的にスキップする。
    let norm_bare = |lang: crate::language::LangId, n: &str| -> String {
        crate::language::normalize_identifier(lang, bare_name(n)).into_owned()
    };

    // 同名 export が複数ファイル/複数コンテナに存在する場合は保守的にスキップ（誤判定防止）。
    // キーは bare name を言語別に正規化したもの (Xojo では `Foo` と `FOO` を同一視)。
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (name, _, _, lang) in &all_syms {
        *name_counts.entry(norm_bare(*lang, name)).or_default() += 1;
    }

    // 全シンボル名の非 Definition 参照件数をカウント（SymbolReference を確保しない）。
    // 入力も正規化済みキーで渡し、refs 側の HashMap キーと lookup を一致させる。
    let unique_names: Vec<String> = {
        let mut seen = HashSet::new();
        all_syms
            .iter()
            .map(|(name, _, _, lang)| norm_bare(*lang, name))
            .filter(|n| seen.insert(n.clone()))
            .collect()
    };

    let counts = match crate::engine::refs::count_non_definition_refs_batch(
        &unique_names,
        &canonical_dir,
        glob,
    ) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Android プロジェクトでは `AndroidManifest.xml` / layout XML から
    // シンボルが参照されうる（`<activity android:name=".MainActivity"/>` 等）。
    // Kotlin/Java AST のみでは追跡できない Android framework 経由の生存判定を補うため、
    // XML 参照集合に含まれるシンボルは dead から除外する。
    // AndroidManifest.xml が存在しないプロジェクトでは空集合が返り副作用なし。
    let xml_refs = crate::engine::xml_refs::collect_xml_symbol_references(&canonical_dir);

    // 定義以外の参照が0件のシンボルを dead として報告
    let mut dead = Vec::new();
    for (name, kind, file, lang) in &all_syms {
        let key = norm_bare(*lang, name);
        // 同名シンボルが複数存在する場合は bare name では区別できないためスキップ
        if name_counts.get(&key).copied().unwrap_or(0) > 1 {
            continue;
        }

        if counts.get(&key).copied().unwrap_or(0) == 0 {
            // bare name と qualname (Container.method) の両方を XML 参照と突き合わせる。
            // layout XML の `android:onClick="handler"` は単純名でしか書けないため bare で検索し、
            // `android:name=".Foo"` 等で Container 側をカバーするケースは qualname でも検査する。
            let bare = bare_name(name);
            if xml_refs.contains(bare) || xml_refs.contains(name.as_str()) {
                continue;
            }
            dead.push(DeadSymbol {
                name: name.clone(),
                kind: kind.clone(),
                file: file.clone(),
            });
        }
    }

    dead
}

fn extract_exported_symbols_from_git(
    dir: &str,
    base: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
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

    let source = &output.stdout;
    let utf8_path = camino::Utf8Path::new(file_path);
    let lang_id = crate::language::LangId::from_path(utf8_path).ok()?;
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;
    // Rust の `impl Trait for Type` 配下のメソッドは trait の実装事実であり、独立した
    // 公開 API item ではない。module 移動など実体は維持したままの変更でも api.add / api.rm
    // に誤計上されるのを避けるため、API 変更検出でも trait impl メソッドを除外する。
    Some(filter_exported_symbols(&syms, root, source, lang_id, true))
}

fn extract_exported_symbols_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    extract_exported_symbols_from_file_inner(dir, file_path, true)
}

fn extract_dead_code_candidates_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    extract_exported_symbols_from_file_inner(dir, file_path, true)
}

fn extract_exported_symbols_from_file_inner(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
) -> Option<Vec<(String, String, String)>> {
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = camino::Utf8Path::new(full_path.to_str()?);
    let source = parser::read_file(utf8_path).ok()?;
    let lang_id = crate::language::LangId::from_path(utf8_path).ok()?;
    let tree = parser::parse_source(&source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, &source, lang_id).ok()?;
    Some(filter_exported_symbols(
        &syms,
        root,
        &source,
        lang_id,
        exclude_trait_impls,
    ))
}

/// シンボルの種類に応じた API シグネチャを抽出する。
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 宣言行のみ。
///
/// クラス/型は宣言行（`class Foo(Bar):` や `struct Foo {` など）のみをシグネチャとする。
/// 本体（メソッド本体や private フィールド）の変更でクラス全体の API 変更として
/// 再検出されるのを避けるため、メンバーの集約はしない。
/// メンバー個々の変更は method シンボル単独で検出される。
fn extract_api_signature(sym: &crate::models::symbol::Symbol, lines: &[&str]) -> String {
    lines
        .get(sym.range.start.line)
        .unwrap_or(&"")
        .trim()
        .to_string()
}

fn filter_exported_symbols(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
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
        // PHPUnit 規約のテストメソッド / テストクラス。PHP 限定。
        // `public function testXxx`, `setUp`, `tearDown`, `setUpBeforeClass`,
        // `tearDownAfterClass`, および `*Test` / `*TestCase` / `*IntegrationTest` /
        // `*FeatureTest` クラスは PHPUnit のランナーから自動で呼ばれる規約的シンボルで、
        // 識別子レベルの cross-file ref は発生しないが dead でもない。
        if is_phpunit_test_symbol(&sym.name, sym.kind, lang_id) {
            continue;
        }
        let sig = extract_api_signature(sym, &lines);
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
    refs_result.references.iter().any(|r| {
        if r.kind == Some(RefKind::Definition) {
            return false;
        }
        let ref_path = r.path.as_str();
        std::path::Path::new(ref_path) != defining_path && diff_new_paths.contains(ref_path)
    })
}

/// `file_path` が属する Rust crate が binary-only (`src/lib.rs` を持たず外部から
/// `pub` シンボルへ到達できない構成) かを判定する。binary-only crate では `pub` は
/// クレート内モジュール境界の役割しか持たないため api.add の対象から除外する。
///
/// 判定方針: `file_path` (dir 相対) から祖先方向に遡って最も近い `Cargo.toml` を
/// 見つけ、そのディレクトリに `src/lib.rs` が存在しなければ binary-only とみなす。
/// Rust ファイル以外や `Cargo.toml` が見つからない場合は false を返す。
fn is_binary_only_rust_crate(dir: &str, file_path: &str) -> bool {
    let path = std::path::Path::new(file_path);
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    let full = std::path::Path::new(dir).join(file_path);
    let dir_canonical = std::fs::canonicalize(dir).ok();
    let mut current = full.parent();
    while let Some(d) = current {
        if d.join("Cargo.toml").is_file() {
            return !d.join("src").join("lib.rs").is_file();
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
fn is_removed_symbol_unreferenced(dir: &str, name: &str) -> bool {
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(name, dir, None) else {
        return false;
    };
    refs_result.references.is_empty()
}

/// `sym` の range を内包する最も内側の container (class/struct/trait/interface/enum) を返す。
/// `sym` 自身は除外する。
fn enclosing_container<'a>(
    sym: &crate::models::symbol::Symbol,
    containers: &'a [&'a crate::models::symbol::Symbol],
) -> Option<&'a crate::models::symbol::Symbol> {
    let s = sym.range.start.line;
    let e = sym.range.end.line;
    containers
        .iter()
        .copied()
        .filter(|c| {
            let cs = c.range.start.line;
            let ce = c.range.end.line;
            cs <= s && ce >= e && !(cs == s && ce == e)
        })
        .min_by_key(|c| c.range.end.line.saturating_sub(c.range.start.line))
}

// ---------------------------------------------------------------------------
// Dead-code コマンド: diff 関連 or プロジェクト全体のデッドコード検出
// ---------------------------------------------------------------------------

/// `dead-code` の既定除外ディレクトリ名。
///
/// 大規模リポでは `vendor/`, `node_modules/`, `tests/` 等が `dead-code` 候補の 88%+ を占め、
/// 実運用のノイズになる。ディレクトリ名と完全一致するセグメントをパスに含むファイルを
/// 走査対象から落とす。`--include-vendor` / `--include-tests` / `--include-build` で
/// 個別に再取込できる。
///
/// グループ化の意図:
/// - `vendor`: Composer, Ruby Bundler, Go modules vendor
/// - `node_modules`, `bower_components`: Node パッケージ
/// - `tests`, `Tests`, `__tests__`, `spec`, `testdata`: 言語共通のテストディレクトリ
/// - `target`, `dist`, `build`, `out`, `_build`, `cmake-build-debug`, `cmake-build-release`: ビルド成果物
/// - `.venv`, `venv`, `.tox`: Python 仮想環境
const DEFAULT_DEAD_CODE_EXCLUDES_VENDOR: &[&str] = &[
    "vendor",
    "node_modules",
    "bower_components",
    ".venv",
    "venv",
    ".tox",
];
const DEFAULT_DEAD_CODE_EXCLUDES_TESTS: &[&str] =
    &["tests", "Tests", "__tests__", "spec", "testdata"];
const DEFAULT_DEAD_CODE_EXCLUDES_BUILD: &[&str] = &[
    "target",
    "dist",
    "build",
    "out",
    "_build",
    "cmake-build-debug",
    "cmake-build-release",
];

/// 現在のフラグ設定から除外ディレクトリリストを組み立てる。
fn resolve_dead_code_excludes(
    include_vendor: bool,
    include_tests: bool,
    include_build: bool,
) -> Vec<&'static str> {
    let mut excludes: Vec<&'static str> = Vec::new();
    if !include_vendor {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_VENDOR);
    }
    if !include_tests {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_TESTS);
    }
    if !include_build {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_BUILD);
    }
    excludes
}

/// 指定パスが既定除外対象のディレクトリセグメントを含むかを判定する。
fn path_is_default_excluded(path: &str, excludes: &[&'static str]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    path.split('/').any(|seg| excludes.contains(&seg))
}

/// PHPUnit 命名規約に合致するシンボルかどうかを判定する。
///
/// PHP プロジェクト (Laravel を含む) ではテストメソッドは `public function testXxx` や
/// `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` 等、PHPUnit が自動で
/// 呼び出す規約的メソッドが大半。識別子レベルの cross-file ref は生じないが dead ではない。
///
/// 同じ規約は JUnit / NUnit / MSTest でも使われるが誤判定を避けるため、本判定は PHP
/// ファイルに限定する。
fn is_phpunit_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Php {
        return false;
    }
    // qualname (`Foo.testBar`) の末尾要素を取る
    let short = name.rsplit_once('.').map(|(_, t)| t).unwrap_or(name);
    match kind {
        SymbolKind::Class => {
            short.ends_with("Test")
                || short.ends_with("TestCase")
                || short.ends_with("IntegrationTest")
                || short.ends_with("FeatureTest")
        }
        SymbolKind::Method | SymbolKind::Function => {
            matches!(
                short,
                "setUp" | "tearDown" | "setUpBeforeClass" | "tearDownAfterClass"
            ) || is_phpunit_test_method_name(short)
        }
        _ => false,
    }
}

/// `^test[A-Z_]` で始まるメソッド名かどうか (PHPUnit の testXxx 規約)。
fn is_phpunit_test_method_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() <= 4 {
        return false;
    }
    if &bytes[..4] != b"test" {
        return false;
    }
    let c = bytes[4];
    c.is_ascii_uppercase() || c == b'_'
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_dead_code(
    dir: &str,
    glob: Option<&str>,
    diff: Option<&str>,
    diff_file: Option<&str>,
    git: bool,
    base: &str,
    staged: bool,
    include_vendor: bool,
    include_tests: bool,
    include_build: bool,
    pretty: bool,
) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    if !canonical_dir.is_dir() {
        return Err(
            AstroError::new(ErrorCode::InvalidRequest, format!("Not a directory: {dir}")).into(),
        );
    }

    let excludes = resolve_dead_code_excludes(include_vendor, include_tests, include_build);

    // diff 指定があれば diff 関連ファイルのみ、なければプロジェクト全体
    let has_diff = diff.is_some() || diff_file.is_some() || git;
    let files: Vec<std::path::PathBuf> = if has_diff {
        let diff_input = if let Some(d) = diff {
            d.to_string()
        } else if let Some(df) = diff_file {
            read_file_to_string_limited(df, MAX_INPUT_SIZE)?
        } else {
            run_git_diff(dir, base, staged)?
        };

        if diff_input.trim().is_empty() {
            let result = DeadCodeResult {
                dir: canonical_dir.to_string_lossy().to_string(),
                scanned_files: 0,
                dead_symbols: Vec::new(),
            };
            let output = serialize_output(&result, pretty)?;
            println!("{output}");
            return Ok(());
        }

        let diff_files = crate::engine::diff::parse_unified_diff(&diff_input);
        let mut files: Vec<std::path::PathBuf> = diff_files
            .iter()
            .filter(|f| f.new_path != "/dev/null")
            .map(|f| canonical_dir.join(&f.new_path))
            .filter(|p| {
                // パース可能な言語のファイルのみ対象
                crate::language::LangId::from_path(camino::Utf8Path::new(p.to_str().unwrap_or("")))
                    .is_ok()
            })
            .filter(|p| {
                // 既定除外ディレクトリ配下は dead-code 対象から落とす
                !path_is_default_excluded(&p.to_string_lossy(), &excludes)
            })
            .collect();
        // glob フィルタが指定されていれば適用（不正パターンはエラー）
        if let Some(pattern) = glob {
            let mut ob = ignore::overrides::OverrideBuilder::new(&canonical_dir);
            ob.add(pattern)?;
            let overrides = ob.build()?;
            files.retain(|p| overrides.matched(p, false).is_whitelist());
        }
        files
    } else {
        crate::engine::refs::collect_files_with_excludes(&canonical_dir, glob, &excludes)?
    };

    let scanned_files = files.len();
    let dead_symbols = detect_dead_symbols_from_files(dir, &files, glob);

    let result = DeadCodeResult {
        dir: canonical_dir.to_string_lossy().to_string(),
        scanned_files,
        dead_symbols,
    };

    let output = serialize_output(&result, pretty)?;
    info!(
        command = "dead-code",
        dir = dir,
        scanned_files = scanned_files,
        dead_count = result.dead_symbols.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch processing (NDJSON output, rayon parallel)
// ---------------------------------------------------------------------------

/// NDJSON バッチ出力: 並列処理の結果をインデックス順に stdout へ書き出す。
/// `par_iter().collect::<Vec<String>>()` は中間 Vec に全結果を保持して
/// ピーク RSS が膨張するため、完了済みスロットを別スレッドで随時排出する。
/// 入力順と一致する出力を保つ（既存テストの期待値）。
fn batch_ndjson<F>(paths: &[String], process: F) -> Result<()>
where
    F: Fn(&str) -> String + Sync,
{
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    if paths.is_empty() {
        return Ok(());
    }

    let batch_size = paths.len();
    // 各スロットは排出時に `take` されるため Mutex<Option<String>>
    let slots: Vec<Mutex<Option<String>>> = (0..batch_size).map(|_| Mutex::new(None)).collect();
    let (tx, rx) = mpsc::channel::<usize>();
    let next_to_write = AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        let slots_ref = &slots;
        let next_to_write_ref = &next_to_write;

        let writer = scope.spawn(move || -> Result<usize> {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let mut bytes = 0usize;
            // 完了通知を受け取り、次に書くべきインデックスが揃っている間は順次排出する
            for _ in rx {
                loop {
                    let cur = next_to_write_ref.load(Ordering::Acquire);
                    if cur >= batch_size {
                        break;
                    }
                    let taken = {
                        let mut guard = slots_ref[cur].lock().expect("slot mutex poisoned");
                        guard.take()
                    };
                    if let Some(line) = taken {
                        bytes += line.len() + 1;
                        writeln!(out, "{line}")?;
                        next_to_write_ref.store(cur + 1, Ordering::Release);
                    } else {
                        break;
                    }
                }
            }
            Ok(bytes)
        });

        paths
            .par_iter()
            .enumerate()
            .for_each_with(tx, |tx, (i, p)| {
                let line = process(p);
                *slots_ref[i].lock().expect("slot mutex poisoned") = Some(line);
                let _ = tx.send(i);
            });

        let written = writer
            .join()
            .map_err(|_| anyhow!("batch_ndjson writer thread panicked"))??;
        info!(
            batch_size = batch_size,
            output_bytes = written,
            "batch completed"
        );
        Ok(())
    })
}

pub fn batch_ast(
    service: &AppService,
    paths: &[String],
    depth: usize,
    context_lines: usize,
    full: bool,
) -> Result<()> {
    batch_ndjson(paths, |p| {
        let params = AstParams {
            path: p,
            line: None,
            col: None,
            end_line: None,
            end_col: None,
            depth,
            context_lines,
        };
        match service.extract_ast(&params) {
            Ok(response) => {
                if full {
                    serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
                } else {
                    serde_json::to_string(&response.to_compact_ast())
                        .unwrap_or_else(|e| make_error_line(&e.into()))
                }
            }
            Err(e) => make_error_line(&e),
        }
    })
}

pub fn batch_symbols(
    service: &AppService,
    paths: &[String],
    doc: bool,
    full: bool,
    dir: Option<&std::path::Path>,
) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_symbols(p) {
        Ok(mut response) => {
            // dir 指定時に絶対パスを相対パスに変換
            if let Some(base) = dir
                && let Ok(rel) = std::path::Path::new(&response.location.path).strip_prefix(base)
            {
                response.location.path = rel.to_string_lossy().to_string();
            }
            if full {
                serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
            } else {
                let compact = response.to_compact_symbols(doc);
                serde_json::to_string(&compact).unwrap_or_else(|e| make_error_line(&e.into()))
            }
        }
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_calls(service: &AppService, paths: &[String], function: Option<&str>) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, |p| match service.extract_calls(p, func.as_deref()) {
        Ok(result) => serde_json::to_string(&result.to_compact())
            .unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_imports(service: &AppService, paths: &[String]) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_imports(p) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_lint(
    service: &AppService,
    paths: &[String],
    rules: &[crate::models::lint::Rule],
) -> Result<()> {
    batch_ndjson(paths, |p| match service.lint_file(p, rules) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_sequence(
    service: &AppService,
    paths: &[String],
    function: Option<&str>,
) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, |p| {
        match service.generate_sequence(p, func.as_deref()) {
            Ok(result) => {
                serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into()))
            }
            Err(e) => make_error_line(&e),
        }
    })
}

// ---------------------------------------------------------------------------
// Session handler
// ---------------------------------------------------------------------------

pub fn handle_request(
    service: &AppService,
    req: crate::models::request::AstgenRequest,
) -> Result<serde_json::Value> {
    use crate::models::request::Command;

    match req.command {
        Command::Ast => {
            let params = AstParams {
                path: &req.path,
                line: req.line,
                col: req.column,
                end_line: req.end_line,
                end_col: req.end_column,
                depth: req.depth.unwrap_or(3),
                context_lines: req.context_lines.unwrap_or(3),
            };
            let response = service.extract_ast(&params)?;
            Ok(serde_json::to_value(response)?)
        }
        Command::Symbols => {
            let response = service.extract_symbols(&req.path)?;
            let compact = response.to_compact_symbols(false);
            Ok(serde_json::to_value(compact)?)
        }
        Command::Doctor => {
            let report = doctor::run_doctor();
            Ok(serde_json::to_value(report)?)
        }
        Command::Calls => {
            let result = service.extract_calls(&req.path, req.function.as_deref())?;
            Ok(serde_json::to_value(result.to_compact())?)
        }
        Command::Refs => {
            let dir = req.dir.as_deref().unwrap_or(".");
            if let Some(names) = &req.names {
                let filtered: Vec<String> = names
                    .iter()
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect();
                if filtered.is_empty() {
                    return Err(AstroError::new(
                        ErrorCode::InvalidRequest,
                        "One of name or names is required",
                    )
                    .into());
                }
                let results = service.find_references_batch(&filtered, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(results)?)
            } else if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty())
            {
                let result = service.find_references(name, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(result)?)
            } else {
                Err(AstroError::new(
                    ErrorCode::InvalidRequest,
                    "One of name or names is required",
                )
                .into())
            }
        }
        Command::Context => {
            let dir = req.dir.as_deref().unwrap_or(".");
            let diff_input = req.diff.as_deref().unwrap_or("");
            let result = service.analyze_context(diff_input, dir)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Imports => {
            let result = service.extract_imports(&req.path)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Lint => {
            let rules = req.rules.as_deref().unwrap_or(&[]);
            let result = service.lint_file(&req.path, rules)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Sequence => {
            let result = service.generate_sequence(&req.path, req.function.as_deref())?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Cochange => {
            let dir = req.dir.as_deref().unwrap_or(".");
            let defaults = CoChangeOptions::default();
            let opts = CoChangeOptions {
                lookback: req.lookback.unwrap_or(defaults.lookback),
                min_confidence: req.min_confidence.unwrap_or(defaults.min_confidence),
                min_samples: req.min_samples.unwrap_or(defaults.min_samples),
                max_files_per_commit: req
                    .max_files_per_commit
                    .unwrap_or(defaults.max_files_per_commit),
                bounded_by_merge_base: req
                    .bounded_by_merge_base
                    .unwrap_or(defaults.bounded_by_merge_base),
                skip_deleted_files: req
                    .skip_deleted_files
                    .unwrap_or(defaults.skip_deleted_files),
                filter_file: req.file.clone(),
            };
            let result = service.analyze_cochange(dir, &opts)?;
            Ok(serde_json::to_value(result)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::process::Command;

    #[test]
    fn read_to_string_limited_accepts_small_input() {
        let text = read_to_string_limited(Cursor::new(b"ok".to_vec()), 4, "stdin").unwrap();
        assert_eq!(text, "ok");
    }

    #[test]
    fn read_to_string_limited_rejects_oversized_input() {
        let err = read_to_string_limited(Cursor::new(b"abcde".to_vec()), 4, "stdin")
            .expect_err("oversized input should fail");

        assert!(err.to_string().contains("exceeds maximum size"));
    }

    #[test]
    fn read_bytes_limited_and_drain_reports_full_size() {
        let err = read_bytes_limited_and_drain(Cursor::new(vec![b'a'; 10]), 4, "git diff output")
            .expect_err("oversized input should fail");

        assert!(err.to_string().contains("10 bytes > 4 bytes"));
    }

    #[test]
    fn read_to_string_limited_rejects_invalid_utf8() {
        let err = read_to_string_limited(Cursor::new(vec![0xff]), 4, "stdin")
            .expect_err("invalid utf-8 should fail");

        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn validate_git_revision_accepts_normal_values() {
        assert!(validate_git_revision("HEAD", "--base").is_ok());
        assert!(validate_git_revision("HEAD^", "--base").is_ok());
        assert!(validate_git_revision("main", "--base").is_ok());
        assert!(validate_git_revision("origin/main", "--base").is_ok());
        assert!(validate_git_revision("feature/foo", "--base").is_ok());
        assert!(validate_git_revision("abc1234", "--base").is_ok());
        assert!(validate_git_revision("v1.0.0", "--base").is_ok());
    }

    // `--output=/path` 等のオプション注入を拒否する
    #[test]
    fn validate_git_revision_rejects_option_prefix() {
        let err = validate_git_revision("--output=/tmp/pwn", "--base")
            .expect_err("option-like base should be rejected");
        assert!(err.to_string().contains("must not start with '-'"));

        let err =
            validate_git_revision("-p", "--base").expect_err("short option should be rejected");
        assert!(err.to_string().contains("must not start with '-'"));
    }

    #[test]
    fn validate_git_revision_rejects_empty() {
        let err =
            validate_git_revision("", "--base").expect_err("empty revision should be rejected");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_git_revision_rejects_nul() {
        let err =
            validate_git_revision("HEAD\0foo", "--base").expect_err("NUL byte should be rejected");
        assert!(err.to_string().contains("must not contain NUL"));
    }

    #[test]
    fn detect_api_changes_uses_old_path_for_renamed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        assert!(
            Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(repo)
                .status()
                .expect("git init")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.name", "astro-sight-tests"])
                .current_dir(repo)
                .status()
                .expect("git config user.name")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.email", "astro-sight@example.com"])
                .current_dir(repo)
                .status()
                .expect("git config user.email")
                .success()
        );

        let old_path = src_dir.join("old.rs");
        fs::write(&old_path, "pub fn greet() -> i32 {\n    1\n}\n").expect("write old file");

        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "initial"])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );

        let new_path = src_dir.join("new.rs");
        fs::rename(&old_path, &new_path).expect("rename file");
        fs::write(
            &new_path,
            "pub fn greet(name: &str) -> i32 {\n    name.len() as i32\n}\n",
        )
        .expect("write renamed file");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/old.rs".to_string(),
            new_path: "src/new.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        assert!(
            api_changes
                .modified
                .iter()
                .any(|change| change.name == "greet"
                    && change.old_signature.as_deref() == Some("pub fn greet() -> i32 {")
                    && change.new_signature.as_deref()
                        == Some("pub fn greet(name: &str) -> i32 {")),
            "rename を含む差分でも関数シグネチャ変更を検出するべき"
        );
    }

    /// テストヘルパー: 一時 git リポジトリを初期化する。
    fn init_git_repo_for_test(repo: &std::path::Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "astro-sight-tests"],
            vec!["config", "user.email", "astro-sight@example.com"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(repo)
                    .status()
                    .expect("git")
                    .success()
            );
        }
    }

    /// テストヘルパー: 与えられたファイル一覧を書き込み、add + commit する。
    fn git_commit_files(repo: &std::path::Path, files: &[(&str, &str)], msg: &str) {
        for (rel, content) in files {
            let full = repo.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(full, content).expect("write file");
        }
        assert!(
            Command::new("git")
                .args(["add", "-A"])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", msg])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );
    }

    #[test]
    fn detect_api_changes_rename_preserves_symbols() {
        // Python スクリプトを rename した際、同名・同シグネチャの関数は
        // api.rm / api.add として報告されないことを確認する（レポートの再現シナリオ）。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let old_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def build_entries():
    return []

def regenerate():
    return None

def main():
    pass
";
        git_commit_files(
            repo,
            &[("scripts/regenerate_marketplace.py", old_content)],
            "initial",
        );

        // 旧ファイル削除 + 新ファイル追加 (git mv と同じ効果)
        fs::remove_file(repo.join("scripts/regenerate_marketplace.py")).expect("rm old");
        let new_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def build_entries():
    return []

def regenerate():
    return None

def main():
    pass
";
        fs::write(repo.join("scripts/marketplace.py"), new_content).expect("write new");

        // git の rename detection で単一 DiffFile として扱われる場合
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "scripts/regenerate_marketplace.py".to_string(),
            new_path: "scripts/marketplace.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 14,
                new_start: 1,
                new_count: 14,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            added.is_empty(),
            "rename で保持された関数は api.add に出るべきではない。got: {added:?}"
        );
        assert!(
            removed.is_empty(),
            "rename で保持された関数は api.rm に出るべきではない。got: {removed:?}"
        );
    }

    #[test]
    fn detect_api_changes_reconciles_delete_and_add_as_rename() {
        // git diff が rename を検出できず、旧ファイル削除 + 新ファイル追加の
        // 2 エントリとして供給された場合でも、同一シグネチャの関数は相殺される。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let old_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def main():
    pass
";
        git_commit_files(
            repo,
            &[("scripts/regenerate_marketplace.py", old_content)],
            "initial",
        );

        // ファイル削除 + 別パスに再配置 (rename detection が無効な想定)
        fs::remove_file(repo.join("scripts/regenerate_marketplace.py")).expect("rm old");
        let new_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def main():
    pass

def new_public_api():
    return 1
";
        fs::write(repo.join("scripts/marketplace.py"), new_content).expect("write new");

        // rename 未検出の diff: delete + add の 2 エントリ
        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "scripts/regenerate_marketplace.py".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 9,
                    new_start: 0,
                    new_count: 0,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "scripts/marketplace.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 12,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added_names: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        let removed_names: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        // 同一シグネチャの 3 関数は相殺される
        assert!(
            !removed_names.contains(&"iter_plugin_manifests"),
            "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
        );
        assert!(
            !removed_names.contains(&"check_layout"),
            "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
        );
        assert!(
            !removed_names.contains(&"main"),
            "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
        );
        assert!(
            !added_names.contains(&"iter_plugin_manifests"),
            "相殺済みの関数は added にも現れるべきではない。got added: {added_names:?}"
        );

        // ただし純粋な新規関数は api.add に残る
        assert!(
            added_names.contains(&"new_public_api"),
            "新規追加された関数は引き続き検出されるべき。got added: {added_names:?}"
        );
    }

    #[test]
    fn detect_api_changes_still_detects_genuine_removal() {
        // リネームではなく純粋に関数を削除した場合は api.rm が発報される。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(
            repo,
            &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
            "initial",
        );
        // bar を削除
        fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "mod.py".to_string(),
            new_path: "mod.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 2,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"bar"),
            "純粋な関数削除は api.rm として検出されるべき。got: {removed:?}"
        );
    }

    #[test]
    fn detect_api_changes_skips_linguist_generated_files() {
        // .gitattributes で linguist-generated 指定されたファイルの API 変更は報告しない。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(
            repo,
            &[
                (".gitattributes", "generated.py linguist-generated\n"),
                ("generated.py", "def old_gen():\n    pass\n"),
                ("hand.py", "def old_hand():\n    pass\n"),
            ],
            "initial",
        );
        // 生成ファイルと手書きファイルの双方で関数追加
        fs::write(
            repo.join("generated.py"),
            "def old_gen():\n    pass\n\ndef new_gen():\n    pass\n",
        )
        .expect("write");
        fs::write(
            repo.join("hand.py"),
            "def old_hand():\n    pass\n\ndef new_hand():\n    pass\n",
        )
        .expect("write");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "generated.py".to_string(),
                new_path: "generated.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 5,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "hand.py".to_string(),
                new_path: "hand.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 5,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !added.contains(&"new_gen"),
            "linguist-generated ファイルの API 変更は除外されるべき。got: {added:?}"
        );
        assert!(
            added.contains(&"new_hand"),
            "通常ファイルの API 追加は検出されるべき。got: {added:?}"
        );
    }

    /// ファイル先頭に自動生成マーカーコメント (`@generated` / `Automatically generated
    /// by ...`) を含むファイルは、.gitattributes が無くても API 変更検出から除外される。
    /// (レポート 2026-04-16-tree-sitter-generated-enum-dead-code.md の再現)
    #[test]
    fn detect_api_changes_skips_auto_generated_marker_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(
            repo,
            &[
                (
                    "gen.py",
                    "# @generated by tree-sitter\ndef old_gen():\n    pass\n",
                ),
                ("hand.py", "def old_hand():\n    pass\n"),
            ],
            "initial",
        );
        fs::write(
            repo.join("gen.py"),
            "# @generated by tree-sitter\ndef old_gen():\n    pass\n\ndef new_gen():\n    pass\n",
        )
        .expect("write");
        fs::write(
            repo.join("hand.py"),
            "def old_hand():\n    pass\n\ndef new_hand():\n    pass\n",
        )
        .expect("write");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "gen.py".to_string(),
                new_path: "gen.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 6,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "hand.py".to_string(),
                new_path: "hand.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 5,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !added.contains(&"new_gen"),
            "@generated マーカーのあるファイルは API 変更検出から除外されるべき。got: {added:?}"
        );
        assert!(
            added.contains(&"new_hand"),
            "通常ファイルの API 追加は検出されるべき。got: {added:?}"
        );
    }

    /// dead-code 検出でも同じマーカーで生成ファイルは除外される
    #[test]
    fn detect_dead_symbols_skips_auto_generated_marker_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        fs::write(
            repo.join("gen.py"),
            "# Automatically generated by tree-sitter\ndef unused_gen():\n    pass\n",
        )
        .expect("write");
        fs::write(repo.join("hand.py"), "def unused_hand():\n    pass\n").expect("write");

        let files = vec![repo.join("gen.py"), repo.join("hand.py")];
        let dead = detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files, None);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"unused_gen"),
            "自動生成マーカーのあるファイルは dead-code 検出から除外されるべき。got: {names:?}"
        );
        assert!(
            names.contains(&"unused_hand"),
            "通常ファイルの未使用関数は dead として検出されるべき。got: {names:?}"
        );
    }

    /// Rust の `pub mod foo;` 宣言追加は api.add に出してはならない。
    /// モジュール宣言はファイル構成の整理であり、公開 API 面としての意味が薄いため
    /// `filter_exported_symbols` で `SymbolKind::Module` を除外している。
    /// (Stop hook 改善時に導入。`extract_all_callees` 追加コミットで Stop hook が
    /// `pub mod generated;` を api.add 通知した問題の再発防止)
    #[test]
    fn detect_api_changes_skips_module_declaration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
pub mod existing;
pub fn hello() {}
";
        git_commit_files(repo, &[("src/lib.rs", before)], "initial");

        // 新規モジュール宣言を追加 (副ファイルは存在しなくても tree-sitter パースには影響しない)
        let after = "\
pub mod existing;
pub mod generated;
pub fn hello() {}
";
        fs::write(repo.join("src/lib.rs"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 3,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !added.contains(&"generated"),
            "pub mod 追加は api.add に出してはならない。got: {added:?}"
        );
        assert!(
            !added.contains(&"existing"),
            "既存 pub mod も api.add に出してはならない。got: {added:?}"
        );
    }

    /// Rust の `pub struct` へ private フィールドを追加しただけでは api.mod に出ない。
    /// 宣言行 (`pub struct Foo {`) は変わらず、本体 (フィールド) の変更のため
    /// `extract_api_signature` が宣言行のみを見る既存のロジックで自然に除外される。
    /// (レポート 2026-04-17-private-field-addition-over-detection.md の再現)
    #[test]
    fn detect_api_changes_private_field_addition_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
}
";
        git_commit_files(repo, &[("src/lib.rs", before)], "initial");

        // private フィールド追加のみ（pub struct 宣言行は不変）
        let after = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
    codex_reasoning_effort: String,
}
";
        fs::write(repo.join("src/lib.rs"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 3,
                old_count: 1,
                new_start: 3,
                new_count: 2,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"AiService"),
            "pub struct の内部（private フィールド）変更は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// Python で同名メソッドを持つ複数クラスがあるとき、qualname (`ClassName.method`)
    /// として区別され、触っていない方は api.mod に出ない。
    #[test]
    fn detect_api_changes_distinguishes_same_named_python_methods() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
class ClaudeReviewer:
    def execute(self) -> int:
        return 1


class CodexReviewer:
    def execute(self) -> str:
        return \"ok\"


class ReReviewExecutor:
    def execute(self) -> None:
        pass
";
        git_commit_files(repo, &[("svc.py", before)], "initial");

        // ReReviewExecutor.execute だけ本体を変更（シグネチャは同じ）
        let after = "\
class ClaudeReviewer:
    def execute(self) -> int:
        return 1


class CodexReviewer:
    def execute(self) -> str:
        return \"ok\"


class ReReviewExecutor:
    def execute(self) -> None:
        return None
";
        fs::write(repo.join("svc.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "svc.py".to_string(),
            new_path: "svc.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 13,
                old_count: 1,
                new_start: 13,
                new_count: 1,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        // bare name `execute` は重複検出されず、qualname で区別されていること
        assert!(
            mod_names.iter().all(|n| *n != "execute"),
            "bare name `execute` は出ないはず（qualname 化されているべき）。got: {mod_names:?}"
        );
        // シグネチャ変更なし（本体のみ変更）なので api.mod には何も出ないはず
        assert!(
            api_changes.modified.is_empty(),
            "本体のみの変更で signature 不変なら modified に出ないはず。got: {:?}",
            api_changes.modified
        );
    }

    /// Python クラスの private メソッドの本体変更は、クラス自体の modified として上がらない。
    /// 宣言行（`class Foo:`）が変わらない限り Class のシグネチャは不変。
    #[test]
    fn detect_api_changes_class_body_change_does_not_mark_class_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
class PromptBuilder:
    def _build_common(self) -> str:
        return \"v1\"
";
        git_commit_files(repo, &[("pb.py", before)], "initial");

        let after = "\
class PromptBuilder:
    def _build_common(self) -> str:
        return \"v2 with much more text\"
";
        fs::write(repo.join("pb.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "pb.py".to_string(),
            new_path: "pb.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 3,
                old_count: 1,
                new_start: 3,
                new_count: 1,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        assert!(
            !mod_names.contains(&"PromptBuilder"),
            "クラス本体の変更でクラス自体を api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// Python で同一クラス内のメソッドシグネチャが変わった場合は qualname で検出される。
    #[test]
    fn detect_api_changes_detects_qualified_method_signature_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
class Reviewer:
    def execute(self) -> int:
        return 1
";
        git_commit_files(repo, &[("r.py", before)], "initial");

        let after = "\
class Reviewer:
    def execute(self, mode: str) -> int:
        return 1
";
        fs::write(repo.join("r.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "r.py".to_string(),
            new_path: "r.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 2,
                old_count: 1,
                new_start: 2,
                new_count: 1,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        assert!(
            mod_names.contains(&"Reviewer.execute"),
            "qualname 形式のメソッドシグネチャ変更を検出すべき。got: {mod_names:?}"
        );
    }

    /// Bash スクリプトで同一ファイル内から呼ばれている新規関数は api.add に出ない。
    /// (レポート 2026-04-17-api-add-bash-connected-function-false-positive.md)
    #[test]
    fn detect_api_changes_bash_internally_called_function_is_not_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
sparse_clone_or_update() {\n    echo clone\n}\n\n\
for repo in \"foo\"; do\n    sparse_clone_or_update\ndone\n";
        git_commit_files(repo, &[("sp.sh", before)], "initial");

        // sparse_patterns_for を新規追加し、同ファイル内の sparse_clone_or_update から呼び出す
        let after = "#!/usr/bin/env bash\n\
sparse_patterns_for() {\n    echo pattern\n}\n\n\
sparse_clone_or_update() {\n    sparse_patterns_for\n    echo clone\n}\n\n\
for repo in \"foo\"; do\n    sparse_clone_or_update\ndone\n";
        fs::write(repo.join("sp.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "sp.sh".to_string(),
            new_path: "sp.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 8,
                new_start: 1,
                new_count: 11,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.contains(&"sparse_patterns_for"),
            "同一ファイル内から呼ばれている Bash 関数は api.add に出してはならない。got: {added:?}"
        );
    }

    /// Bash で同一ファイル内から呼ばれていない新規関数は api.add に残る。
    #[test]
    fn detect_api_changes_bash_disconnected_function_is_still_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
        git_commit_files(repo, &[("sp.sh", before)], "initial");

        // 新規関数 unused_helper は誰も呼んでいない
        let after = "#!/usr/bin/env bash\n\
unused_helper() {\n    echo unused\n}\n\n\
main() {\n    echo hi\n}\nmain\n";
        fs::write(repo.join("sp.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "sp.sh".to_string(),
            new_path: "sp.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 7,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            added.contains(&"unused_helper"),
            "同一ファイル内から呼ばれていない新規関数は api.add に残すべき。got: {added:?}"
        );
    }

    /// Python で同一ファイル内から呼ばれている新規 public 関数は api.add に出ない。
    #[test]
    fn detect_api_changes_python_internally_called_function_is_not_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "def main():\n    print(\"hi\")\n";
        git_commit_files(repo, &[("svc.py", before)], "initial");

        // helper を追加し、main から呼ぶ
        let after = "def helper() -> str:\n    return \"x\"\n\n\
def main():\n    helper()\n    print(\"hi\")\n";
        fs::write(repo.join("svc.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "svc.py".to_string(),
            new_path: "svc.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 6,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.contains(&"helper"),
            "同一ファイル内で呼ばれている Python 関数は api.add に出してはならない。got: {added:?}"
        );
    }

    /// Python CLI スクリプト（同一ファイル内でのみ呼ばれる関数）のシグネチャ変更は
    /// caller が同じ diff 内で追随できるため api.mod に出さない。
    /// (レポート 2026-04-22-closed-in-diff-signature-change-noise.md の再現)
    #[test]
    fn detect_api_changes_python_cli_signature_change_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
def run_osv_scanner(path: str) -> int:
    return 0


def scan_worktree(path: str) -> int:
    rc = run_osv_scanner(path)
    return rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
        git_commit_files(repo, &[("osv_scan.py", before)], "initial");

        // run_osv_scanner の戻り値型を int -> tuple[int, float] に変更。
        // caller (scan_worktree) も同じ diff 内で追随する。
        let after = "\
def run_osv_scanner(path: str) -> tuple[int, float]:
    return (0, 0.0)


def scan_worktree(path: str) -> int:
    _rc, _elapsed = run_osv_scanner(path)
    return _rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
        fs::write(repo.join("osv_scan.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "osv_scan.py".to_string(),
            new_path: "osv_scan.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 11,
                new_start: 1,
                new_count: 11,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"run_osv_scanner"),
            "同一ファイル内でのみ呼ばれる関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// Bash の `trap <fn> SIGNAL` で参照される関数は、同一ファイル内で cleanup
    /// ハンドラとして使われるだけのため api.add に出してはならない。
    /// (レポート 2026-04-21-bash-trap-exit-handler-false-positive.md の再現)
    #[test]
    fn detect_api_changes_bash_trap_handler_is_not_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
echo initial\n";
        git_commit_files(repo, &[("run_review.sh", before)], "initial");

        // 新規に cleanup ハンドラを追加し、trap でのみ参照する
        let after = "#!/usr/bin/env bash\n\
stop_memory_sampler() {\n    echo stop\n}\n\n\
trap stop_memory_sampler EXIT\n\
echo initial\n";
        fs::write(repo.join("run_review.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "run_review.sh".to_string(),
            new_path: "run_review.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 7,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.contains(&"stop_memory_sampler"),
            "trap <fn> EXIT でのみ参照される bash 関数は api.add に出してはならない。got: {added:?}"
        );
    }

    /// Bash の内部ヘルパー関数（同一ファイル内でのみ呼ばれる）のシグネチャ変更も
    /// api.mod に出さない（パターン A と対称）。
    #[test]
    fn detect_api_changes_bash_internal_signature_change_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
timed() {\n    \"$@\"\n}\n\n\
main() {\n    timed echo hi\n}\nmain\n";
        git_commit_files(repo, &[("run.sh", before)], "initial");

        // timed の宣言行を変更（シグネチャ変更相当）
        let after = "#!/usr/bin/env bash\n\
timed() { # wrap with timing\n    \"$@\"\n}\n\n\
main() {\n    timed echo hi\n}\nmain\n";
        fs::write(repo.join("run.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "run.sh".to_string(),
            new_path: "run.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 2,
                old_count: 1,
                new_start: 2,
                new_count: 1,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"timed"),
            "同一ファイル内でのみ呼ばれる bash 関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// 他ファイルから参照される関数のシグネチャ変更は api.mod に残す（false negative 防止）。
    /// 同一ファイル内でも呼び出しが存在するが、他ファイルから import/call されている場合は
    /// closed-in-diff とは言えないため、レビュー対象として残す必要がある。
    #[test]
    fn detect_api_changes_externally_called_signature_change_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let lib_before = "\
def run(value: int) -> int:
    return value


def wrapper() -> int:
    return run(1)
";
        let caller_before = "\
from lib import run


def main() -> int:
    return run(2)
";
        git_commit_files(
            repo,
            &[("lib.py", lib_before), ("caller.py", caller_before)],
            "initial",
        );

        // lib.run のシグネチャを変更（引数追加）。caller.py は diff に含まれない（追随なし）。
        let lib_after = "\
def run(value: int, flag: bool = False) -> int:
    return value


def wrapper() -> int:
    return run(1, False)
";
        fs::write(repo.join("lib.py"), lib_after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "lib.py".to_string(),
            new_path: "lib.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"run"),
            "他ファイルから参照される関数のシグネチャ変更は api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// 後方互換なオプショナル引数の追加（末尾にデフォルト値付き引数を追加）は、
    /// closed-in-diff 判定により api.mod から除外される。
    /// (レポート追記 2026-04-22 コミット c045fdf `json_to_markdown` の再現)
    #[test]
    fn detect_api_changes_optional_arg_addition_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
def json_to_markdown(raw, impact_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw)


if __name__ == \"__main__\":
    _finalize_result({})
";
        git_commit_files(repo, &[("review_mr.py", before)], "initial");

        let after = "\
def json_to_markdown(raw, impact_file=None, osv_scan_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw, impact_file=None, osv_scan_file=None)


if __name__ == \"__main__\":
    _finalize_result({})
";
        fs::write(repo.join("review_mr.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "review_mr.py".to_string(),
            new_path: "review_mr.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 10,
                new_start: 1,
                new_count: 10,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"json_to_markdown"),
            "同一ファイル内でのみ呼ばれる関数へのオプショナル引数追加は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// CLI スクリプト内で関数を rename + 実装置換した場合、api.rm に残してはならない。
    /// `api.rm { old_name }` + `api.add { new_name }` の両方が closed-in-diff として
    /// 扱えることを確認する。
    /// (レポート追記 2026-04-22 コミット 3f2b082 `detect_changed_manifests` の再現)
    #[test]
    fn detect_api_changes_rename_with_impl_replacement_not_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "\
def detect_changed_manifests(base, head):
    return []


def main():
    files = detect_changed_manifests(\"a\", \"b\")
    return files


if __name__ == \"__main__\":
    main()
";
        git_commit_files(repo, &[("osv_scan.py", before)], "initial");

        // detect_changed_manifests を削除し、同じ diff 内で list_changed_files を追加。
        // caller (main) も list_changed_files に追随。
        let after = "\
def list_changed_files(base, head):
    return []


def main():
    files = list_changed_files(\"a\", \"b\")
    return files


if __name__ == \"__main__\":
    main()
";
        fs::write(repo.join("osv_scan.py"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "osv_scan.py".to_string(),
            new_path: "osv_scan.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 10,
                new_start: 1,
                new_count: 10,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !removed.contains(&"detect_changed_manifests"),
            "同一 diff 内で新規関数に切り替わった関数の削除は api.rm に出してはならない。got: {removed:?}"
        );
        // 新規関数側も is_internally_connected により除外される（main から呼ばれている）。
        assert!(
            !added.contains(&"list_changed_files"),
            "同一ファイル内でのみ呼ばれる新規関数は api.add に出してはならない。got: {added:?}"
        );
    }

    /// 2026-04-24 レポート再現: binary crate (src/lib.rs なし) で新規 pub struct を
    /// 追加し、同一 diff 内の別ファイルから use で取り込むケース。gitlab-cli の `MrDiff`
    /// 追加と同じ構造。binary-only crate のため api.add の対象外になるべき。
    #[test]
    fn detect_api_changes_binary_rust_crate_excludes_pub_additions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"demo-bin\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
";
        let models_before = "pub struct Issue { pub id: u32 }\n";
        let main_before = "\
use crate::models::Issue;

fn main() {
    let _ = Issue { id: 1 };
}

mod models;
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/models.rs", models_before),
                ("src/main.rs", main_before),
            ],
            "initial",
        );

        // 新規 pub struct MrDiff を models.rs に追加し、main.rs の use に追随させる
        let models_after = "\
pub struct Issue { pub id: u32 }

pub struct MrDiff {
    pub old_path: String,
    pub new_path: String,
}
";
        let main_after = "\
use crate::models::{Issue, MrDiff};

fn main() {
    let _ = Issue { id: 1 };
    let _ = MrDiff { old_path: String::new(), new_path: String::new() };
}

mod models;
";
        fs::write(repo.join("src/models.rs"), models_after).expect("write models");
        fs::write(repo.join("src/main.rs"), main_after).expect("write main");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/models.rs".to_string(),
                new_path: "src/models.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 6,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "src/main.rs".to_string(),
                new_path: "src/main.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 8,
                    new_start: 1,
                    new_count: 8,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.contains(&"MrDiff"),
            "binary crate (src/lib.rs なし) の新規 pub struct は api.add に出してはならない。got: {added:?}"
        );
    }

    /// library crate (src/lib.rs あり) では新規 pub シンボルを api.add に残す。
    /// binary crate 判定の副作用で library crate のシンボルまで消さないことを保証する。
    #[test]
    fn detect_api_changes_library_rust_crate_keeps_pub_additions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod models;\n";
        let models_before = "pub struct Issue { pub id: u32 }\n";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/models.rs", models_before),
            ],
            "initial",
        );

        // library crate に新規 pub struct を追加（同一 diff 内では参照しない）
        let models_after = "\
pub struct Issue { pub id: u32 }

pub struct LibraryApi { pub name: String }
";
        fs::write(repo.join("src/models.rs"), models_after).expect("write models");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/models.rs".to_string(),
            new_path: "src/models.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 4,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            added.contains(&"LibraryApi"),
            "library crate (src/lib.rs あり) の新規 pub struct は api.add に残すべき。got: {added:?}"
        );
    }

    /// lib.rs 有りクレートでも、新規 pub シンボルが同一 diff 内の別ファイルから
    /// 参照されていれば api.add から除外する。
    #[test]
    fn detect_api_changes_library_used_in_same_diff_excluded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod models;\npub mod consumer;\n";
        let models_before = "pub struct Issue { pub id: u32 }\n";
        let consumer_before = "use crate::models::Issue;\n\npub fn use_issue(i: Issue) {}\n";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/models.rs", models_before),
                ("src/consumer.rs", consumer_before),
            ],
            "initial",
        );

        // models に新規 pub struct を追加し、同一 diff 内で consumer.rs から参照
        let models_after = "\
pub struct Issue { pub id: u32 }

pub struct MrDiff { pub path: String }
";
        let consumer_after = "\
use crate::models::{Issue, MrDiff};

pub fn use_issue(i: Issue) {}
pub fn use_diff(d: MrDiff) {}
";
        fs::write(repo.join("src/models.rs"), models_after).expect("write models");
        fs::write(repo.join("src/consumer.rs"), consumer_after).expect("write consumer");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/models.rs".to_string(),
                new_path: "src/models.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 4,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "src/consumer.rs".to_string(),
                new_path: "src/consumer.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 5,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.contains(&"MrDiff"),
            "同一 diff 内で参照される新規 pub struct は api.add から除外すべき。got: {added:?}"
        );
    }

    // ------------------------------------------------------------------
    // is_internally_connected ヘルパー
    // ------------------------------------------------------------------

    #[test]
    fn is_internally_connected_matches_bare_name() {
        let mut callees = std::collections::HashSet::new();
        callees.insert("foo".to_string());
        assert!(is_internally_connected(&callees, "foo"));
        assert!(!is_internally_connected(&callees, "bar"));
    }

    #[test]
    fn is_internally_connected_matches_qualname_via_bare() {
        let mut callees = std::collections::HashSet::new();
        // Python/Ruby 等では callee 側は bare name のみになることが多い
        callees.insert("execute".to_string());
        assert!(is_internally_connected(&callees, "Reviewer.execute"));
    }

    #[test]
    fn is_internally_connected_does_not_match_disjoint() {
        let mut callees = std::collections::HashSet::new();
        callees.insert("other_fn".to_string());
        assert!(!is_internally_connected(&callees, "Reviewer.execute"));
        assert!(!is_internally_connected(&callees, "execute"));
    }

    // ------------------------------------------------------------------
    // is_binary_only_rust_crate ヘルパー
    // ------------------------------------------------------------------

    #[test]
    fn is_binary_only_rust_crate_true_when_no_lib_rs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = \"b\"\n").expect("cargo");
        fs::create_dir_all(repo.join("src")).expect("mkdir src");
        fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("main");

        assert!(is_binary_only_rust_crate(
            repo.to_str().expect("utf-8"),
            "src/main.rs",
        ));
    }

    #[test]
    fn is_binary_only_rust_crate_false_when_lib_rs_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = \"l\"\n").expect("cargo");
        fs::create_dir_all(repo.join("src")).expect("mkdir src");
        fs::write(repo.join("src/lib.rs"), "pub fn public_api() {}\n").expect("lib");

        assert!(!is_binary_only_rust_crate(
            repo.to_str().expect("utf-8"),
            "src/lib.rs",
        ));
    }

    #[test]
    fn is_binary_only_rust_crate_false_for_non_rust_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = \"b\"\n").expect("cargo");

        assert!(!is_binary_only_rust_crate(
            repo.to_str().expect("utf-8"),
            "src/main.py",
        ));
    }

    #[test]
    fn is_binary_only_rust_crate_false_without_cargo_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        fs::create_dir_all(repo.join("src")).expect("mkdir src");
        fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("main");

        assert!(!is_binary_only_rust_crate(
            repo.to_str().expect("utf-8"),
            "src/main.rs",
        ));
    }

    #[test]
    fn reconcile_api_symbols_pairs_by_signature() {
        // reconcile_api_symbols のユニットテスト: 同じ (name,kind,sig) を相殺し、
        // 残りだけを返す。
        let added = vec![
            ApiSymbolCandidate {
                name: "foo".into(),
                kind: "function".into(),
                file: "new.py".into(),
                signature: "def foo():".into(),
            },
            ApiSymbolCandidate {
                name: "new_api".into(),
                kind: "function".into(),
                file: "new.py".into(),
                signature: "def new_api():".into(),
            },
        ];
        let removed = vec![
            ApiSymbolCandidate {
                name: "foo".into(),
                kind: "function".into(),
                file: "old.py".into(),
                signature: "def foo():".into(),
            },
            ApiSymbolCandidate {
                name: "gone".into(),
                kind: "function".into(),
                file: "old.py".into(),
                signature: "def gone():".into(),
            },
        ];

        let (kept_added, kept_removed) = reconcile_api_symbols(added, removed);
        assert_eq!(kept_added.len(), 1);
        assert_eq!(kept_added[0].name, "new_api");
        assert_eq!(kept_removed.len(), 1);
        assert_eq!(kept_removed[0].name, "gone");
    }

    #[test]
    fn reconcile_api_symbols_keeps_different_signatures() {
        // 同名でもシグネチャが違うなら相殺しない（signature change の検出漏れ防止）。
        let added = vec![ApiSymbolCandidate {
            name: "foo".into(),
            kind: "function".into(),
            file: "b.py".into(),
            signature: "def foo(x):".into(),
        }];
        let removed = vec![ApiSymbolCandidate {
            name: "foo".into(),
            kind: "function".into(),
            file: "a.py".into(),
            signature: "def foo():".into(),
        }];

        let (kept_added, kept_removed) = reconcile_api_symbols(added, removed);
        assert_eq!(kept_added.len(), 1);
        assert_eq!(kept_removed.len(), 1);
    }

    #[test]
    fn detect_api_changes_skips_python_private_helpers() {
        // Python: `_` プレフィックスのヘルパーを public リファクタで追加しても
        // api.add として通知されないことを確認する（レポートの再現シナリオ）
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();

        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "astro-sight-tests"],
            vec!["config", "user.email", "astro-sight@example.com"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(repo)
                    .status()
                    .expect("git")
                    .success()
            );
        }

        let script_path = repo.join("tool.py");
        fs::write(&script_path, "def check_layout():\n    return True\n").expect("write old file");

        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "initial"])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );

        // 拡張: private helper 2 個と public helper 1 個を追加
        fs::write(
            &script_path,
            r#"def _add_error(msg):
    return msg

def _check_plugin_manifest(path):
    return _add_error(path)

def check_layout():
    return _check_plugin_manifest("x")

def new_public_api():
    return 1
"#,
        )
        .expect("write new file");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "tool.py".to_string(),
            new_path: "tool.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 11,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added_names: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !added_names.contains(&"_add_error"),
            "Python の `_` プレフィックス関数は api.add から除外されるべき。got: {added_names:?}"
        );
        assert!(
            !added_names.contains(&"_check_plugin_manifest"),
            "Python の `_` プレフィックス関数は api.add から除外されるべき。got: {added_names:?}"
        );
        assert!(
            added_names.contains(&"new_public_api"),
            "`_` プレフィックスを持たない関数は引き続き api.add として検出されるべき。got: {added_names:?}"
        );
    }

    #[test]
    fn detect_api_changes_rename_removed_uses_old_path() {
        // ファイルリネーム時にシンボルが削除された場合、removed の file は
        // 旧パス (old_path) を使用することを確認する。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();

        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[(
                "src/old.rs",
                "pub fn greet() -> i32 {\n    1\n}\n\npub fn farewell() -> i32 {\n    0\n}\n",
            )],
            "initial",
        );

        // リネーム後のファイルから farewell を削除
        let new_path = repo.join("src/new.rs");
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&new_path, "pub fn greet() -> i32 {\n    1\n}\n").expect("write renamed file");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/old.rs".to_string(),
            new_path: "src/new.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 3,
            }],
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_farewell = api_changes.removed.iter().find(|s| s.name == "farewell");

        assert!(
            removed_farewell.is_some(),
            "farewell が removed に含まれるべき。got: {:?}",
            api_changes.removed
        );

        assert_eq!(
            removed_farewell.unwrap().file,
            "src/old.rs",
            "削除シンボルの file は旧パス (old_path) であるべき"
        );
    }

    #[test]
    fn detect_api_changes_ignores_moved_trait_impl_methods() {
        // Rust の `impl Trait for Type` 配下の trait メソッドは実装事実であり、
        // 独立した公開 API item として扱うべきではない。`impl` ブロックをファイル間で
        // 移動しただけで `api.rm` / `api.add` に出るのは誤検出。
        // 本テストは mod.rs を複数サブモジュールに分割する際に `on_ref` / `default` が
        // api.rm へ漏れ出していた実例 (2026-04-21 トリアージ) の回帰防止。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // 初期: a.rs に struct Foo と impl Default for Foo
        git_commit_files(
            repo,
            &[(
                "src/a.rs",
                "pub struct Foo;\n\nimpl Default for Foo {\n    fn default() -> Self {\n        Self\n    }\n}\n",
            )],
            "initial",
        );

        // 変更: impl Default for Foo を b.rs に移動 (struct は a.rs に残す)
        fs::write(repo.join("src/a.rs"), "pub struct Foo;\n").expect("rewrite a.rs");
        fs::write(
            repo.join("src/b.rs"),
            "use super::a::Foo;\n\nimpl Default for Foo {\n    fn default() -> Self {\n        Self\n    }\n}\n",
        )
        .expect("write b.rs");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/a.rs".to_string(),
                new_path: "src/a.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 7,
                    new_start: 1,
                    new_count: 1,
                }],
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/b.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 7,
                }],
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_has_default = api_changes
            .removed
            .iter()
            .any(|s| s.name.ends_with("default"));
        let added_has_default = api_changes
            .added
            .iter()
            .any(|s| s.name.ends_with("default"));

        assert!(
            !removed_has_default,
            "impl Default for Foo の default メソッドは trait impl であり \
             api.rm に計上すべきでない。got removed: {:?}",
            api_changes.removed
        );
        assert!(
            !added_has_default,
            "impl Default for Foo の default メソッドは trait impl であり \
             api.add に計上すべきでない。got added: {:?}",
            api_changes.added
        );
    }

    #[test]
    fn build_review_hook_json_returns_none_when_no_issues() {
        let dir = tempfile::tempdir().expect("tempdir");

        let build = build_review_hook_json(
            &empty_review_result(),
            dir.path().to_str().expect("utf-8 path"),
        );
        assert!(
            build.value.is_none(),
            "問題がない review 結果では hook JSON を生成しないべき"
        );
        assert!(!build.is_blocking, "出力なしなら blocking にしないべき");
    }

    /// cochange のみの場合は出力はするが exit 1 にはしない (informational)
    #[test]
    fn build_review_hook_json_cochange_only_is_informational() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
            },
            missing_cochanges: vec![MissingCochange {
                file: "a.rs".to_string(),
                expected_with: "b.rs".to_string(),
                confidence: 0.9,
            }],
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
        };

        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"));
        assert!(
            build.value.is_some(),
            "cochange は情報提供として JSON 出力はするべき"
        );
        assert!(
            !build.is_blocking,
            "cochange のみの場合は Stop hook を止めないべき"
        );
    }

    /// api.add のみの場合は informational として出力されるが blocking にはしない
    #[test]
    fn build_review_hook_json_api_add_only_is_informational() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: vec![ApiSymbol {
                    name: "foo".to_string(),
                    kind: "function".to_string(),
                    file: "a.rs".to_string(),
                }],
                removed: Vec::new(),
                modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
        };

        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"));
        assert!(build.value.is_some(), "api.add は hook JSON に出すべき");
        assert!(
            !build.is_blocking,
            "api.add のみ (additive) は Stop hook を止めないべき"
        );
    }

    /// api.removed は破壊的変更の可能性があるため blocking になる
    #[test]
    fn build_review_hook_json_api_removed_is_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: vec![ApiSymbol {
                    name: "foo".to_string(),
                    kind: "function".to_string(),
                    file: "a.rs".to_string(),
                }],
                modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
        };

        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"));
        assert!(build.value.is_some(), "api.rm は hook JSON に出すべき");
        assert!(build.is_blocking, "api.rm は blocking にすべき");
    }

    /// api.modified は破壊的変更の可能性があるため blocking になる
    #[test]
    fn build_review_hook_json_api_modified_is_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: vec![ApiSymbolChange {
                    name: "foo".to_string(),
                    kind: "function".to_string(),
                    file: "a.rs".to_string(),
                    old_signature: Some("fn foo()".to_string()),
                    new_signature: Some("fn foo(x: u32)".to_string()),
                }],
            },
            dead_symbols: Vec::new(),
        };

        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"));
        assert!(build.value.is_some(), "api.mod は hook JSON に出すべき");
        assert!(build.is_blocking, "api.mod は blocking にすべき");
    }

    #[test]
    fn build_review_hook_json_uses_changed_symbols_in_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::write(src_dir.join("lib.rs"), "pub fn compute() {}\n").expect("write changed file");
        fs::write(src_dir.join("main.rs"), "fn main() { compute(); }\n").expect("write caller");

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: vec![crate::models::impact::FileImpact {
                    path: "src/lib.rs".to_string(),
                    hunks: Vec::new(),
                    affected_symbols: vec![crate::models::impact::AffectedSymbol {
                        name: "compute".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    }],
                    signature_changes: Vec::new(),
                    impacted_callers: vec![crate::models::impact::ImpactedCaller {
                        path: "src/main.rs".to_string(),
                        name: "main".to_string(),
                        line: 1,
                        symbols: vec!["main".to_string()],
                    }],
                }],
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
        };

        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"));
        let hook_json = build.value.expect("hook json should be generated");
        assert!(build.is_blocking, "impacts があれば blocking にすべき");
        let impacts = hook_json["impacts"]
            .as_array()
            .expect("impacts should be an array");
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0]["src"], "src/lib.rs");
        assert_eq!(impacts[0]["syms"], serde_json::json!(["compute"]));
        assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["main"]));
    }

    // ------------------------------------------------------------------
    // is_dependency_manifest_pair
    // ------------------------------------------------------------------

    #[test]
    fn is_dependency_manifest_pair_matches_cargo() {
        assert!(is_dependency_manifest_pair("Cargo.toml", "Cargo.lock"));
        assert!(is_dependency_manifest_pair("Cargo.lock", "Cargo.toml"));
    }

    #[test]
    fn is_dependency_manifest_pair_matches_node_lockfiles() {
        for lock in ["package-lock.json", "pnpm-lock.yaml", "yarn.lock"] {
            assert!(
                is_dependency_manifest_pair("package.json", lock),
                "package.json ↔ {lock} should match"
            );
        }
    }

    #[test]
    fn is_dependency_manifest_pair_matches_other_ecosystems() {
        let pairs = [
            ("pyproject.toml", "uv.lock"),
            ("pyproject.toml", "poetry.lock"),
            ("pyproject.toml", "pdm.lock"),
            ("Gemfile", "Gemfile.lock"),
            ("composer.json", "composer.lock"),
            ("go.mod", "go.sum"),
            ("mix.exs", "mix.lock"),
        ];
        for (a, b) in pairs {
            assert!(is_dependency_manifest_pair(a, b), "{a} ↔ {b} should match");
        }
    }

    #[test]
    fn is_dependency_manifest_pair_rejects_unrelated_files() {
        assert!(!is_dependency_manifest_pair("src/lib.rs", "Cargo.toml"));
        assert!(!is_dependency_manifest_pair("Cargo.toml", "README.md"));
        assert!(!is_dependency_manifest_pair(
            "package.json",
            "tsconfig.json"
        ));
    }

    #[test]
    fn is_dependency_manifest_pair_rejects_cross_directory_pairs() {
        // monorepo: 異なるディレクトリのマニフェスト/ロックは別プロジェクトなので除外対象外
        assert!(!is_dependency_manifest_pair(
            "apps/web/package.json",
            "apps/api/package-lock.json"
        ));
        assert!(!is_dependency_manifest_pair(
            "crates/foo/Cargo.toml",
            "crates/bar/Cargo.lock"
        ));
    }

    #[test]
    fn is_dependency_manifest_pair_accepts_same_directory_pairs() {
        assert!(is_dependency_manifest_pair(
            "apps/web/package.json",
            "apps/web/package-lock.json"
        ));
        assert!(is_dependency_manifest_pair(
            "crates/foo/Cargo.toml",
            "crates/foo/Cargo.lock"
        ));
    }

    // ------------------------------------------------------------------
    // detect_missing_cochanges: 依存マニフェスト/ロックペアを除外する
    // ------------------------------------------------------------------

    /// Cargo.toml ↔ Cargo.lock が過去繰り返し共変更されていても
    /// Cargo.lock のみの変更で missing_cochange 警告を出さない。
    #[test]
    fn detect_missing_cochanges_excludes_cargo_manifest_lock_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // Cargo.toml と Cargo.lock を何度も共変更（cochange 統計を作る）
        for i in 0..4 {
            git_commit_files(
                repo,
                &[
                    ("Cargo.toml", &format!("# v{i}\n")),
                    ("Cargo.lock", &format!("# lock v{i}\n")),
                ],
                &format!("dep update {i}"),
            );
        }

        let service = AppService::new();
        let mut changed_files = HashSet::new();
        // Cargo.lock のみが変更された状況（cargo update -p 相当）
        changed_files.insert("Cargo.lock".to_string());

        let missing = detect_missing_cochanges(
            &service,
            repo.to_str().expect("utf-8 path"),
            &changed_files,
            0.3,
        );

        assert!(
            missing.iter().all(|m| m.file != "Cargo.toml"),
            "Cargo.toml が missing_cochange に含まれてはならない。got: {missing:?}"
        );
    }
}
