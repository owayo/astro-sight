use anyhow::{Result, anyhow};
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

pub fn run_git_diff(dir: &str, base: &str, staged: bool) -> Result<String> {
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

fn build_review_hook_json(result: &ReviewResult, dir: &str) -> Option<serde_json::Value> {
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
    let mut has_issues = false;

    // impacts: [{src,syms,refs:[{p,ln,s}]}]
    if !unresolved.is_empty() {
        has_issues = true;
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

    // cochange: [{f,w,c}]
    if !result.missing_cochanges.is_empty() {
        has_issues = true;
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
    if has_api_changes {
        has_issues = true;
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
        has_issues = true;
        let dead: Vec<serde_json::Value> = result
            .dead_symbols
            .iter()
            .map(|ds| serde_json::json!({"n": ds.name, "f": ds.file}))
            .collect();
        hook_obj.insert("dead".into(), serde_json::Value::Array(dead));
    }

    if !has_issues {
        return None;
    }

    hook_obj.insert(
        "hint".into(),
        serde_json::Value::String("False positives? Run astro-sight-triage skill.".into()),
    );

    Some(serde_json::Value::Object(hook_obj))
}

/// --hook 時の review 出力: compact JSON を stderr に出力し exit 1
fn review_hook_output(result: &ReviewResult, dir: &str) -> Result<()> {
    let Some(hook_output) = build_review_hook_json(result, dir) else {
        return Ok(());
    };

    eprintln!("{hook_output}");
    std::process::exit(1);
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

    for df in diff_files {
        // 新旧いずれかが生成物扱いなら API 変更検出の対象外
        if gitattrs.is_generated(&df.new_path) || gitattrs.is_generated(&df.old_path) {
            continue;
        }

        if df.old_path == "/dev/null" {
            if let Some(new_syms) = extract_exported_symbols_from_file(dir, &df.new_path) {
                for (name, kind, sig) in &new_syms {
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

        for (name, kind, sig) in &new_syms {
            if !old_map.contains_key(name.as_str()) {
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
                removed.push(ApiSymbolCandidate {
                    name: name.clone(),
                    kind: kind.clone(),
                    // 削除シンボルの出所は旧ファイルパス
                    file: df.old_path.clone(),
                    signature: sig.clone(),
                });
            }
        }

        for (name, kind, new_sig) in &new_syms {
            if let Some(old_sig) = old_map.get(name.as_str())
                && old_sig != &new_sig.as_str()
            {
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
    let mut all_syms: Vec<(String, String, String)> = Vec::new(); // (name, kind, file)
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
        if let Some(syms) = extract_dead_code_candidates_from_file(dir, &rel) {
            for (name, kind, _sig) in syms {
                all_syms.push((name, kind, rel.clone()));
            }
        }
    }

    if all_syms.is_empty() {
        return Vec::new();
    }

    // 同名 export が複数ファイルに存在する場合は保守的にスキップ（誤判定防止）
    let mut name_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (name, _, _) in &all_syms {
        *name_counts.entry(name.as_str()).or_default() += 1;
    }

    // 全シンボル名の非 Definition 参照件数をカウント（SymbolReference を確保しない）
    let unique_names: Vec<String> = {
        let mut seen = HashSet::new();
        all_syms
            .iter()
            .map(|(name, _, _)| name.clone())
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

    // 定義以外の参照が0件のシンボルを dead として報告
    let mut dead = Vec::new();
    for (name, kind, file) in &all_syms {
        // 同名 export が複数ファイルにある場合は判定不能なのでスキップ
        if name_counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
            continue;
        }

        if counts.get(name).copied().unwrap_or(0) == 0 {
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
    // API 変更検出では trait impl も差分に含めたいので除外しない
    Some(filter_exported_symbols(&syms, root, source, lang_id, false))
}

fn extract_exported_symbols_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    extract_exported_symbols_from_file_inner(dir, file_path, false)
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
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 全行を結合。
fn extract_api_signature(sym: &crate::models::symbol::Symbol, lines: &[&str]) -> String {
    use crate::models::symbol::SymbolKind;
    match sym.kind {
        SymbolKind::Function | SymbolKind::Method => {
            // 関数は先頭行で十分
            lines
                .get(sym.range.start.line)
                .unwrap_or(&"")
                .trim()
                .to_string()
        }
        SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Trait
        | SymbolKind::Interface
        | SymbolKind::Class => {
            // 型はメンバー行を集約してシグネチャとする
            let start = sym.range.start.line;
            let end = sym.range.end.line.min(lines.len().saturating_sub(1));
            let members: Vec<String> = (start..=end)
                .filter_map(|i| {
                    let line = lines.get(i)?.trim();
                    if !line.is_empty() {
                        Some(line.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            members.join("\n")
        }
        _ => lines
            .get(sym.range.start.line)
            .unwrap_or(&"")
            .trim()
            .to_string(),
    }
}

fn filter_exported_symbols(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
) -> Vec<(String, String, String)> {
    let source_str = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = source_str.lines().collect();

    let mut result = Vec::new();
    for sym in syms {
        if !crate::engine::symbols::is_symbol_exported(root, source, lang_id, &sym.range) {
            continue;
        }
        // pub(crate), pub(super) 等はクレート内部APIなので除外
        let decl_line = lines.get(sym.range.start.line).unwrap_or(&"").trim();
        if decl_line.contains("pub(") {
            continue;
        }
        // Rust の trait impl メソッドは trait dispatch 経由で呼ばれ、
        // cross-file refs 検索では caller を辿れない。dead-code 判定では
        // 偽陽性になるためスキップする。API 変更検出では含める。
        if exclude_trait_impls
            && lang_id == crate::language::LangId::Rust
            && crate::engine::symbols::is_trait_impl_method_rust(root, &sym.range)
        {
            continue;
        }
        let sig = extract_api_signature(sym, &lines);
        result.push((
            sym.name.clone(),
            format!("{:?}", sym.kind).to_lowercase(),
            sig,
        ));
    }
    result
}

// ---------------------------------------------------------------------------
// Dead-code コマンド: diff 関連 or プロジェクト全体のデッドコード検出
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn cmd_dead_code(
    dir: &str,
    glob: Option<&str>,
    diff: Option<&str>,
    diff_file: Option<&str>,
    git: bool,
    base: &str,
    staged: bool,
    pretty: bool,
) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    if !canonical_dir.is_dir() {
        return Err(
            AstroError::new(ErrorCode::InvalidRequest, format!("Not a directory: {dir}")).into(),
        );
    }

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
        crate::engine::refs::collect_files(&canonical_dir, glob)?
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

/// NDJSON batch output: process in parallel (order preserved), write with locked stdout.
fn batch_ndjson<F>(paths: &[String], process: F) -> Result<()>
where
    F: Fn(&str) -> String + Sync,
{
    use std::io::Write;
    let results: Vec<String> = paths.par_iter().map(|p| process(p)).collect();
    let total_bytes: usize = results.iter().map(|r| r.len()).sum();
    info!(
        batch_size = paths.len(),
        output_bytes = total_bytes,
        "batch completed"
    );
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in &results {
        writeln!(out, "{line}")?;
    }
    Ok(())
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
    fn build_review_hook_json_returns_none_when_no_issues() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(
            build_review_hook_json(
                &empty_review_result(),
                dir.path().to_str().expect("utf-8 path")
            )
            .is_none(),
            "問題がない review 結果では hook JSON を生成しないべき"
        );
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

        let hook_json = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"))
            .expect("hook json should be generated");
        let impacts = hook_json["impacts"]
            .as_array()
            .expect("impacts should be an array");
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0]["src"], "src/lib.rs");
        assert_eq!(impacts[0]["syms"], serde_json::json!(["compute"]));
        assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["main"]));
    }
}
