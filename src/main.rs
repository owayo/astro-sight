use anyhow::Result;
use clap::Parser;
use tracing::info;

use astro_sight::cli::{Cli, Commands};
use astro_sight::commands::{
    self, CmdAstOpts, batch_ast, batch_calls, batch_imports, batch_lint, batch_sequence,
    batch_symbols, cmd_ast, cmd_calls, cmd_cochange, cmd_context, cmd_dead_code, cmd_doctor,
    cmd_impact, cmd_imports, cmd_lint, cmd_mcp, cmd_refs, cmd_refs_batch, cmd_review, cmd_sequence,
    cmd_session, cmd_symbols, cmd_symbols_dir,
};
use astro_sight::config::ConfigService;
use astro_sight::error::{AstroError, ErrorCode};
use astro_sight::service::AppService;

// dhat-heap feature 有効時のみヒーププロファイラを差し込む。
// 実行後に `dhat-heap.json` が書き出されるので dh_view.html で可視化する。
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// それ以外では mimalloc を採用する。macOS の libmalloc は短命 heap の断片化が
// 激しく、大規模リポジトリの impact 解析で RSS が数 GB に膨らむ主要因になるため。
// mimalloc は thread-local caching と小オブジェクト合体でフットプリントを大きく抑える。
#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // mimalloc に idle ページを即 OS へ返却させる。短命 allocation を大量に行う
    // impact streaming Pass で、chunk 完了後の free が RSS に即反映されるようにする。
    // SAFETY: clap parse よりも前、シングルスレッド状態で設定しているため環境変数
    // の更新は他スレッドと競合しない。
    #[cfg(not(feature = "dhat-heap"))]
    unsafe {
        if std::env::var_os("MI_PURGE_DELAY").is_none() {
            std::env::set_var("MI_PURGE_DELAY", "0");
        }
        if std::env::var_os("MI_ABANDONED_PAGE_PURGE").is_none() {
            std::env::set_var("MI_ABANDONED_PAGE_PURGE", "1");
        }
    }

    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        let (code, message) = commands::classify_error(&e);
        let error = serde_json::json!({
            "error": { "code": code, "message": message }
        });
        println!("{}", serde_json::to_string(&error).unwrap());
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Input resolution helpers
// ---------------------------------------------------------------------------

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
        let trimmed = n.trim();
        if trimmed.is_empty() {
            return Err(
                AstroError::new(ErrorCode::InvalidRequest, "--name must not be empty").into(),
            );
        }
        Ok(NameInput::Single(trimmed.to_string()))
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
        if list.is_empty() {
            return Err(AstroError::new(
                ErrorCode::InvalidRequest,
                "--paths must contain at least one path",
            )
            .into());
        }
        Ok(PathInput::Batch(list))
    } else if let Some(pf) = paths_file {
        let content = std::fs::read_to_string(pf)?;
        let list: Vec<String> = content
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if list.is_empty() {
            return Err(AstroError::new(
                ErrorCode::InvalidRequest,
                "--paths-file must contain at least one path",
            )
            .into());
        }
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
        "🚀 command invoked"
    );

    let service = AppService::new();
    let start = std::time::Instant::now();

    let result = match cli.command {
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
            full,
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
                        full,
                        no_cache,
                        pretty,
                    };
                    cmd_ast(&service, &opts)
                }
                PathInput::Batch(ps) => batch_ast(&service, &ps, depth, context, full),
            }
        }
        Commands::Symbols {
            path,
            paths,
            paths_file,
            dir,
            glob,
            query: _,
            doc,
            full,
            no_cache,
        } => {
            if let Some(d) = &dir {
                cmd_symbols_dir(&service, d, glob.as_deref(), doc, full)
            } else {
                let input =
                    resolve_paths(path.as_deref(), paths.as_deref(), paths_file.as_deref())?;
                match input {
                    PathInput::Single(p) => cmd_symbols(&service, &p, no_cache, pretty, doc, full),
                    PathInput::Batch(ps) => batch_symbols(&service, &ps, doc, full, None),
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
        Commands::Review {
            dir,
            diff,
            diff_file,
            git,
            base,
            staged,
            min_confidence,
            hook,
            framework,
            exclude_dirs,
            exclude_globs,
        } => cmd_review(
            &service,
            &dir,
            diff.as_deref(),
            diff_file.as_deref(),
            git,
            &base,
            staged,
            min_confidence,
            pretty,
            hook,
            framework.as_deref(),
            &exclude_dirs,
            &exclude_globs,
        ),
        Commands::Cochange {
            dir,
            lookback,
            min_confidence,
            min_samples,
            max_files_per_commit,
            no_merge_base,
            include_deleted,
            file,
            blame,
            git,
            base,
            paths,
            paths_file,
            exclude_globs,
        } => {
            // 既定 min_confidence: lookback 0.7 / blame 0.3 (分母セマンティクスが異なるため)
            let resolved_min_confidence = min_confidence.unwrap_or(if blame { 0.3 } else { 0.7 });
            let source_files = if blame {
                astro_sight::commands::resolve_blame_source_files(
                    &dir,
                    git,
                    base.as_deref(),
                    paths.as_deref(),
                    paths_file.as_deref(),
                )?
            } else {
                Vec::new()
            };
            let opts = astro_sight::models::cochange::CoChangeOptions {
                lookback,
                min_confidence: resolved_min_confidence,
                min_samples,
                max_files_per_commit,
                bounded_by_merge_base: !no_merge_base,
                skip_deleted_files: !include_deleted,
                filter_file: file,
                blame,
                source_files,
                base,
                exclude_globs,
            };
            cmd_cochange(&service, &dir, &opts, pretty)
        }
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
        Commands::Impact {
            dir,
            git,
            base,
            staged,
            hook,
        } => cmd_impact(&service, &dir, git, &base, staged, hook),
        Commands::DeadCode {
            dir,
            glob,
            diff,
            diff_file,
            git,
            base,
            staged,
            include_vendor,
            include_tests,
            include_build,
            framework,
            exclude_dirs,
            exclude_globs,
        } => cmd_dead_code(
            &dir,
            glob.as_deref(),
            diff.as_deref(),
            diff_file.as_deref(),
            git,
            &base,
            staged,
            include_vendor,
            include_tests,
            include_build,
            framework.as_deref(),
            &exclude_dirs,
            &exclude_globs,
            pretty,
        ),
        Commands::Doctor => cmd_doctor(pretty),
        Commands::Session => cmd_session(),
        Commands::Mcp => cmd_mcp(),
        Commands::Init { .. } | Commands::SkillInstall { .. } => unreachable!("handled above"),
    };

    let elapsed = start.elapsed();
    info!(
        elapsed_ms = elapsed.as_millis() as u64,
        "⏱️ command finished"
    );

    result
}
