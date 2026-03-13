use anyhow::Result;
use rayon::prelude::*;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::error::{AstroError, ErrorCode};
use crate::service::{AppService, AstParams};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Single-file commands (with cache + pretty support)
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
    lookback: usize,
    min_confidence: f64,
    file: Option<&str>,
    pretty: bool,
) -> Result<()> {
    let result = service.analyze_cochange(dir, lookback, min_confidence, file)?;
    let output = serialize_output(&result, pretty)?;
    info!(command = "cochange", dir = dir, lookback = lookback, min_confidence = min_confidence, file = ?file, output_bytes = output.len(), "command completed");
    println!("{output}");
    Ok(())
}

pub fn run_git_diff(dir: &str, base: &str, staged: bool) -> Result<String> {
    let mut args = vec!["diff".to_string()];
    if staged {
        args.push("--cached".to_string());
    }
    args.push(base.to_string());

    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("git diff failed: {stderr}"),
        )
        .into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
        std::fs::read_to_string(df)?
    } else if git {
        run_git_diff(dir, base, staged)?
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
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
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    if diff_input.trim().is_empty() {
        return Ok(());
    }

    let result = service.analyze_context(&diff_input, dir)?;

    // Collect set of changed file paths
    let changed_paths: std::collections::HashSet<&str> =
        result.changes.iter().map(|c| c.path.as_str()).collect();

    // Group unresolved impacts: callers in files NOT in the diff
    type UnresolvedEntry = (Vec<String>, Vec<(String, usize)>);
    let mut unresolved: std::collections::BTreeMap<String, UnresolvedEntry> =
        std::collections::BTreeMap::new();

    for change in &result.changes {
        let symbols: Vec<String> = change
            .affected_symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if symbols.is_empty() {
            continue;
        }

        for caller in &change.impacted_callers {
            // Resolve relative path against dir for comparison
            let caller_abs = if std::path::Path::new(&caller.path).is_relative() {
                std::path::Path::new(dir)
                    .join(&caller.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                caller.path.clone()
            };

            // Check if caller file is NOT in the changed files
            let in_diff = changed_paths.iter().any(|cp| {
                let cp_abs = if std::path::Path::new(cp).is_relative() {
                    std::path::Path::new(dir)
                        .join(cp)
                        .to_string_lossy()
                        .to_string()
                } else {
                    cp.to_string()
                };
                // Canonicalize both for reliable comparison
                match (
                    std::fs::canonicalize(&caller_abs),
                    std::fs::canonicalize(&cp_abs),
                ) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => caller_abs == cp_abs,
                }
            });

            if !in_diff {
                let entry = unresolved
                    .entry(change.path.clone())
                    .or_insert_with(|| (symbols.clone(), Vec::new()));
                entry.1.push((caller.path.clone(), caller.line));
            }
        }
    }

    if unresolved.is_empty() {
        return Ok(());
    }

    eprintln!("Unresolved impacts found:\n");
    for (changed_path, (symbols, callers)) in &unresolved {
        eprintln!("{} changed [{}]:", changed_path, symbols.join(", "));
        for (caller_path, line) in callers {
            eprintln!("  → {}:{}", caller_path, line);
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
        let transport = rmcp::transport::io::stdio();
        let service = server
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
            // Convert absolute path to relative when dir is specified
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
            let lookback = req.lookback.unwrap_or(200);
            let min_confidence = req.min_confidence.unwrap_or(0.3);
            let result =
                service.analyze_cochange(dir, lookback, min_confidence, req.file.as_deref())?;
            Ok(serde_json::to_value(result)?)
        }
    }
}
