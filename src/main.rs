use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;

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
            query: _,
            no_cache,
        } => {
            let input = resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
            match input {
                PathInput::Single(p) => cmd_symbols(&service, &p, no_cache, pretty),
                PathInput::Batch(ps) => batch_symbols(&service, &ps),
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
        Commands::Refs { name, dir, glob } => {
            cmd_refs(&service, &name, &dir, glob.as_deref(), pretty)
        }
        Commands::Context {
            dir,
            diff,
            diff_file,
        } => cmd_context(
            &service,
            &dir,
            diff.as_deref(),
            diff_file.as_deref(),
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

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, &cache_key, output.as_bytes());
    }

    print!("{output}");
    Ok(())
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
        std::io::Write::write_all(&mut std::io::stdout(), &cached)?;
        return Ok(());
    }

    let response = service.extract_symbols(path)?;

    let mut output = serialize_output(&response, pretty)?;
    output.push('\n');

    if use_cache && let Ok(cache) = CacheStore::new() {
        let _ = cache.put(&hash, "symbols", output.as_bytes());
    }

    print!("{output}");
    Ok(())
}

fn cmd_calls(service: &AppService, path: &str, function: Option<&str>, pretty: bool) -> Result<()> {
    let result = service.extract_calls(path, function)?;
    let output = serialize_output(&result, pretty)?;
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
    println!("{output}");
    Ok(())
}

fn cmd_context(
    service: &AppService,
    dir: &str,
    diff: Option<&str>,
    diff_file: Option<&str>,
    pretty: bool,
) -> Result<()> {
    let diff_input = if let Some(d) = diff {
        d.to_string()
    } else if let Some(df) = diff_file {
        std::fs::read_to_string(df)?
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let result = service.analyze_context(&diff_input, dir)?;
    let output = serialize_output(&result, pretty)?;
    println!("{output}");
    Ok(())
}

fn cmd_doctor(pretty: bool) -> Result<()> {
    let report = doctor::run_doctor();
    let output = serialize_output(&report, pretty)?;
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

fn batch_ast(
    service: &AppService,
    paths: &[String],
    depth: usize,
    context_lines: usize,
) -> Result<()> {
    let results: Vec<String> = paths
        .par_iter()
        .map(|p| {
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
        .collect();

    for line in &results {
        println!("{line}");
    }
    Ok(())
}

fn batch_symbols(service: &AppService, paths: &[String]) -> Result<()> {
    let results: Vec<String> = paths
        .par_iter()
        .map(|p| match service.extract_symbols(p) {
            Ok(response) => {
                serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
            }
            Err(e) => make_error_line(&e),
        })
        .collect();

    for line in &results {
        println!("{line}");
    }
    Ok(())
}

fn batch_calls(service: &AppService, paths: &[String], function: Option<&str>) -> Result<()> {
    let func = function.map(|s| s.to_string());
    let results: Vec<String> = paths
        .par_iter()
        .map(|p| match service.extract_calls(p, func.as_deref()) {
            Ok(result) => {
                serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into()))
            }
            Err(e) => make_error_line(&e),
        })
        .collect();

    for line in &results {
        println!("{line}");
    }
    Ok(())
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
            let name = req.name.as_deref().unwrap_or("");
            let dir = req.dir.as_deref().unwrap_or(".");
            let result = service.find_references(name, dir, req.glob.as_deref())?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Context => {
            let dir = req.dir.as_deref().unwrap_or(".");
            let diff_input = req.diff.as_deref().unwrap_or("");
            let result = service.analyze_context(diff_input, dir)?;
            Ok(serde_json::to_value(result)?)
        }
    }
}
