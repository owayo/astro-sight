use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;

use tracing::info;

use astro_sight::cache::store::CacheStore;
use astro_sight::cli::{Cli, Commands};
use astro_sight::config::ConfigService;
use astro_sight::doctor;
use astro_sight::engine::parser;
use astro_sight::error::{AstroError, ErrorCode};
use astro_sight::service::{AppService, AstParams};
use astro_sight::session;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        let (code, message) = classify_error(&e);
        let error = serde_json::json!({
            "error": { "code": code, "message": message }
        });
        println!("{}", serde_json::to_string(&error).unwrap());
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn classify_error(e: &anyhow::Error) -> (String, String) {
    if let Some(ae) = e.downcast_ref::<AstroError>() {
        (ae.code.to_string(), ae.message.clone())
    } else {
        ("IO_ERROR".to_string(), e.to_string())
    }
}

fn serialize_output(value: &impl serde::Serialize, pretty: bool) -> Result<String> {
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

enum PathInput {
    Single(String),
    Batch(Vec<String>),
}

enum NameInput {
    Single(String),
    Batch(Vec<String>),
}

fn resolve_names(name: Option<&str>, names: Option<&str>) -> Result<NameInput> {
    if let Some(n) = name {
        Ok(NameInput::Single(n.to_string()))
    } else if let Some(ns) = names {
        let list: Vec<String> = ns
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if list.is_empty() {
            return Err(AstroError::new(
                ErrorCode::InvalidRequest,
                "--names must contain at least one symbol name",
            )
            .into());
        }
        Ok(NameInput::Batch(list))
    } else {
        Err(AstroError::new(
            ErrorCode::InvalidRequest,
            "One of --name or --names is required",
        )
        .into())
    }
}

fn resolve_paths(
    path: Option<&str>,
    paths: Option<&str>,
    paths_file: Option<&str>,
) -> Result<PathInput> {
    if let Some(p) = path {
        Ok(PathInput::Single(p.to_string()))
    } else if let Some(ps) = paths {
        let list: Vec<String> = ps
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(PathInput::Batch(list))
    } else if let Some(pf) = paths_file {
        let content = std::fs::read_to_string(pf)?;
        let list: Vec<String> = content
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(PathInput::Batch(list))
    } else {
        Err(AstroError::new(
            ErrorCode::InvalidRequest,
            "One of --path, --paths, or --paths-file is required",
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

fn run(cli: Cli) -> Result<()> {
    let pretty = cli.pretty;

    // Load configuration
    let config = ConfigService::load(cli.config.as_deref())?;

    // Initialize logging if debug mode (CLI flag or config)
    if cli.debug || config.debug {
        astro_sight::logger::init(&config)?;
    }

    // Handle early-exit commands before creating AppService
    match &cli.command {
        Commands::Init { path } => {
            let config_path = if let Some(p) = path {
                ConfigService::generate_at(p)?;
                p.clone()
            } else {
                ConfigService::generate_default()?;
                ConfigService::default_path()
            };
            eprintln!("Configuration file created at: {}", config_path.display());
            return Ok(());
        }
        Commands::SkillInstall { target } => {
            astro_sight::skill::install(target)?;
            return Ok(());
        }
        _ => {}
    }

    // Log command invocation with CWD and input parameters
    let cwd = std::env::current_dir().unwrap_or_default();
    info!(
        command = ?cli.command,
        cwd = %cwd.display(),
        "command invoked"
    );

    let service = AppService::new();

    match cli.command {
        Commands::Ast {
            path,
            paths,
            paths_file,
            line,
            col,
            end_line,
            end_col,
            depth,
            context,
            no_cache,
        } => {
            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => {
                    let opts = CmdAstOpts {
                        path: &p,
                        line,
                        col,
                        end_line,
                        end_col,
                        depth,
                        context_lines: context,
                        no_cache,
                        pretty,
                    };
                    cmd_ast(&service, &opts)
                }
                PathInput::Batch(ps) => batch_ast(&service, &ps, depth, context),
            }
        }
        Commands::Symbols {
            path,
            paths,
            paths_file,
            dir,
            glob,
            query: _,
            no_cache,
        } => {
            if let Some(d) = &dir {
                cmd_symbols_dir(&service, d, glob.as_deref())
            } else {
                let input =
                    resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
                match input {
                    PathInput::Single(p) => cmd_symbols(&service, &p, no_cache, pretty),
                    PathInput::Batch(ps) => batch_symbols(&service, &ps),
                }
            }
        }
        Commands::Calls {
            path,
            paths,
            paths_file,
            function,
        } => {
            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => cmd_calls(&service, &p, function.as_deref(), pretty),
                PathInput::Batch(ps) => batch_calls(&service, &ps, function.as_deref()),
            }
        }
        Commands::Imports {
            path,
            paths,
            paths_file,
        } => {
            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => cmd_imports(&service, &p, pretty),
                PathInput::Batch(ps) => batch_imports(&service, &ps),
            }
        }
        Commands::Lint {
            path,
            paths,
            paths_file,
            rules,
            rules_dir,
        } => {
            let loaded_rules = if let Some(rules_path) = &rules {
                astro_sight::engine::lint::load_rules_from_file(rules_path)?
            } else if let Some(dir) = &rules_dir {
                astro_sight::engine::lint::load_rules_from_dir(dir)?
            } else {
                return Err(AstroError::new(
                    ErrorCode::InvalidRequest,
                    "One of --rules or --rules-dir is required",
                )
                .into());
            };

            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => cmd_lint(&service, &p, &loaded_rules, pretty),
                PathInput::Batch(ps) => batch_lint(&service, &ps, &loaded_rules),
            }
        }
        Commands::Sequence {
            path,
            paths,
            paths_file,
            function,
        } => {
            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => cmd_sequence(&service, &p, function.as_deref(), pretty),
                PathInput::Batch(ps) => batch_sequence(&service, &ps, function.as_deref()),
            }
        }
        Commands::Refs {
            name,
            names,
            dir,
            glob,
        } => match resolve_names(name.as_deref(), names.as_deref())? {
            NameInput::Single(n) => cmd_refs(&service, &n, &dir, glob.as_deref(), pretty),
            NameInput::Batch(ns) => cmd_refs_batch(&service, &ns, &dir, glob.as_deref()),
        },
        Commands::Cochange {
            dir,
            lookback,
            min_confidence,
            file,
        } => cmd_cochange(
            &service,
            &dir,
            lookback,
            min_confidence,
            file.as_deref(),
            pretty,
        ),
        Commands::Context {
            dir,
            diff,
            diff_file,
            git,
            base,
            staged,
        } => cmd_context(
            &service,
            &dir,
            diff.as_deref(),
            diff_file.as_deref(),
            git,
            &base,
            staged,
            pretty,
        ),
        Commands::Doctor => cmd_doctor(pretty),
        Commands::Session => cmd_session(),
        Commands::Mcp => cmd_mcp(),
        Commands::Init { .. } | Commands::SkillInstall { .. } => unreachable!("handled above"),
    }
}

// ---------------------------------------------------------------------------
// Single-file commands (with cache + pretty support)
// ---------------------------------------------------------------------------

struct CmdAstOpts<'a> {
    path: &'a str,
    line: Option<usize>,
    col: Option<usize>,
    end_line: Option<usize>,
    end_col: Option<usize>,
    depth: usize,
    context_lines: usize,
    no_cache: bool,
    pretty: bool,
}

fn cmd_ast(service: &AppService, opts: &CmdAstOpts<'_>) -> Result<()> {
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
    let cache_key = format!(
        "ast_{}_{}_{}_{}_{}_{}",
        opt_key(opts.line),
        opt_key(opts.col),
        opt_key(opts.end_line),
        opt_key(opts.end_col),
        opts.depth,
        opts.context_lines
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
            "command completed"
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

    let mut output = serialize_output(&response, opts.pretty)?;
    output.push('\n');

    info!(
        command = "ast",
        path = opts.path,
        output_bytes = output.len(),
        cached = false,
        "command completed"
    );

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, &cache_key, output.as_bytes());
    }

    print!("{output}");
    Ok(())
}

fn cmd_symbols_dir(service: &AppService, dir: &str, glob: Option<&str>) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    let files = astro_sight::engine::refs::collect_files(&canonical_dir, glob)?;
    let file_paths: Vec<String> = files
        .iter()
        .filter_map(|p| p.to_str().map(|s| s.to_string()))
        .collect();
    batch_symbols(service, &file_paths)
}

fn cmd_symbols(service: &AppService, path: &str, no_cache: bool, pretty: bool) -> Result<()> {
    let utf8_path = camino::Utf8Path::new(path);
    let source = parser::read_file(utf8_path)?;
    let hash = CacheStore::hash(&source);
    let use_cache = !no_cache && !pretty;

    if use_cache
        && let Ok(cache) = CacheStore::new()
        && let Some(cached) = cache.get(&hash, "symbols")
    {
        info!(
            command = "symbols",
            path = path,
            output_bytes = cached.len(),
            cached = true,
            "command completed"
        );
        std::io::Write::write_all(&mut std::io::stdout(), &cached)?;
        return Ok(());
    }

    let response = service.extract_symbols(path)?;

    let mut output = serialize_output(&response, pretty)?;
    output.push('\n');

    info!(
        command = "symbols",
        path = path,
        output_bytes = output.len(),
        cached = false,
        "command completed"
    );

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, "symbols", output.as_bytes());
    }

    print!("{output}");
    Ok(())
}

fn cmd_calls(service: &AppService, path: &str, function: Option<&str>, pretty: bool) -> Result<()> {
    let result = service.extract_calls(path, function)?;
    let output = serialize_output(&result, pretty)?;
    info!(command = "calls", path = path, function = ?function, output_bytes = output.len(), "command completed");
    println!("{output}");
    Ok(())
}

fn cmd_imports(service: &AppService, path: &str, pretty: bool) -> Result<()> {
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

fn cmd_lint(
    service: &AppService,
    path: &str,
    rules: &[astro_sight::models::lint::Rule],
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

fn cmd_sequence(
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

fn cmd_refs(
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

fn cmd_refs_batch(
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

fn cmd_cochange(
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

fn run_git_diff(dir: &str, base: &str, staged: bool) -> Result<String> {
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
fn cmd_context(
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

fn cmd_doctor(pretty: bool) -> Result<()> {
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

fn cmd_session() -> Result<()> {
    let service = AppService::from_env();
    session::run_session(|req| handle_request(&service, req))
}

fn cmd_mcp() -> Result<()> {
    use rmcp::ServiceExt;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let server = astro_sight::mcp::AstroSightServer::new();
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

fn batch_ast(
    service: &AppService,
    paths: &[String],
    depth: usize,
    context_lines: usize,
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
                serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
            }
            Err(e) => make_error_line(&e),
        }
    })
}

fn batch_symbols(service: &AppService, paths: &[String]) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_symbols(p) {
        Ok(response) => {
            serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
        }
        Err(e) => make_error_line(&e),
    })
}

fn batch_calls(service: &AppService, paths: &[String], function: Option<&str>) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, |p| match service.extract_calls(p, func.as_deref()) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

fn batch_imports(service: &AppService, paths: &[String]) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_imports(p) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

fn batch_lint(
    service: &AppService,
    paths: &[String],
    rules: &[astro_sight::models::lint::Rule],
) -> Result<()> {
    batch_ndjson(paths, |p| match service.lint_file(p, rules) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

fn batch_sequence(service: &AppService, paths: &[String], function: Option<&str>) -> Result<()> {
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

fn handle_request(
    service: &AppService,
    req: astro_sight::models::request::AstgenRequest,
) -> Result<serde_json::Value> {
    use astro_sight::models::request::Command;

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
            Ok(serde_json::to_value(response)?)
        }
        Command::Doctor => {
            let report = doctor::run_doctor();
            Ok(serde_json::to_value(report)?)
        }
        Command::Calls => {
            let result = service.extract_calls(&req.path, req.function.as_deref())?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Refs => {
            let dir = req.dir.as_deref().unwrap_or(".");
            if let Some(names) = &req.names {
                // Batch mode
                let results = service.find_references_batch(names, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(results)?)
            } else {
                // Single mode
                let name = req.name.as_deref().unwrap_or("");
                let result = service.find_references(name, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(result)?)
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
