use anyhow::Result;
use clap::Parser;
use std::any::Any;
use std::io::{self, Write};
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

    install_broken_pipe_panic_hook();

    let cli = Cli::parse();

    #[cfg(feature = "dhat-heap")]
    let _profiler = should_start_dhat_heap(&cli.command).then(dhat::Profiler::new_heap);

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let (code, message) = commands::classify_error(&e);
            let error = serde_json::json!({
                "error": { "code": code, "message": message }
            });
            if let Err(write_error) = write_json_error(&error) {
                if write_error.kind() == io::ErrorKind::BrokenPipe {
                    std::process::exit(0);
                }
                eprintln!("failed to write error output: {write_error}");
            }
            std::process::exit(1);
        }
        Err(payload) => {
            if is_broken_pipe_panic_payload(payload.as_ref()) {
                std::process::exit(0);
            }
            std::panic::resume_unwind(payload);
        }
    }
}

fn install_broken_pipe_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_broken_pipe_panic_payload(info.payload()) {
            return;
        }
        default_hook(info);
    }));
}

fn write_json_error(error: &serde_json::Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let line = serde_json::to_string(error).unwrap_or_else(|_| {
        r#"{"error":{"code":"INTERNAL_ERROR","message":"failed to serialize error"}}"#.to_string()
    });
    writeln!(stdout, "{line}")
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> Option<&str> {
    if let Some(message) = payload.downcast_ref::<&str>() {
        Some(message)
    } else {
        payload.downcast_ref::<String>().map(String::as_str)
    }
}

fn is_broken_pipe_panic_payload(payload: &(dyn Any + Send)) -> bool {
    panic_payload_message(payload).is_some_and(|message| {
        message.contains("failed printing to stdout") && message.contains("Broken pipe")
    })
}

#[cfg(feature = "dhat-heap")]
fn should_start_dhat_heap(command: &Commands) -> bool {
    // hook モードは stop hook の出力契約を優先し、DHAT の stderr 要約を混ぜない。
    !matches!(
        command,
        Commands::Impact { hook: true, .. } | Commands::Review { hook: true, .. }
    )
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
        let list = commands::read_paths_file_limited(pf, commands::MAX_INPUT_SIZE)?;
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

    // config を読まずに完結する早期終了コマンドを config ロードより前に処理する。
    // init / skill-install は既存 config を必要としないため、既存 config が壊れて
    // いても動作すべき（特に init は壊れた config の再生成手段になる）。
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

    // 設定をロードする
    let config = ConfigService::load(cli.config.as_deref())?;

    // debug モード時（CLI フラグまたは config）のみロギングを初期化する。
    // 書込不可ディレクトリ等でログ初期化に失敗しても解析本体は止めず、
    // 警告を出して続行する（debug 有効かつ読み取り専用環境などへの堅牢化）。
    if (cli.debug || config.debug)
        && let Err(e) = astro_sight::logger::init(&config)
    {
        eprintln!("warning: failed to initialize logging (continuing without it): {e}");
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
            dead_scope,
            strict_public_const_values,
            include_wip_dead,
        } => {
            // --hook 指定時、未指定なら touched-symbols に降格して
            // 「changed file 内の元から存在した dead」のノイズを抑える。
            let resolved_dead_scope = dead_scope.unwrap_or(if hook {
                astro_sight::cli::DeadScope::TouchedSymbols
            } else {
                astro_sight::cli::DeadScope::All
            });
            cmd_review(
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
                resolved_dead_scope,
                strict_public_const_values,
                include_wip_dead,
            )
        }
        Commands::Cochange {
            dir,
            git,
            base,
            paths,
            paths_file,
            min_confidence,
            min_samples,
            max_files_per_commit,
            commit_size_pivot,
            exclude_globs,
            max_source_files,
            rename,
            copy,
            ignore_merges,
            include_merges,
            max_blame_commits,
            timeout_secs,
            no_smoothing,
            smoothing_alpha,
            smoothing_beta,
            min_denominator,
            per_source_limit,
            author_unit_window_days,
        } => {
            let (source_files, cochange_skip) =
                match astro_sight::commands::resolve_blame_source_files(
                    &dir,
                    git,
                    base.as_deref(),
                    paths.as_deref(),
                    paths_file.as_deref(),
                    &exclude_globs,
                )? {
                    astro_sight::commands::BlameSourceResolution::Files(f) => (f, None),
                    astro_sight::commands::BlameSourceResolution::Skipped(s) => {
                        (Vec::new(), Some(s))
                    }
                };
            // 既定 true。`--include-merges` で旧挙動 (false) に戻す。
            // `--ignore-merges` は no-op として残しているが互換のため明示 ON も尊重する。
            let defaults = astro_sight::models::cochange::CoChangeOptions::default();
            let resolved_ignore_merges = if include_merges {
                false
            } else if ignore_merges {
                true
            } else {
                defaults.ignore_merges
            };
            let opts = astro_sight::models::cochange::CoChangeOptions {
                source_files,
                base,
                min_confidence,
                min_samples,
                max_files_per_commit,
                commit_size_pivot,
                exclude_globs,
                max_source_files,
                rename,
                copy,
                ignore_merges: resolved_ignore_merges,
                max_blame_commits,
                timeout_secs,
                smoothing_alpha,
                smoothing_beta,
                disable_smoothing: no_smoothing,
                min_denominator,
                per_source_limit,
                author_unit_window_days,
            };
            cmd_cochange(&service, &dir, &opts, pretty, cochange_skip)
        }
        Commands::Context {
            dir,
            diff,
            diff_file,
            git,
            base,
            staged,
            exclude_dirs,
            exclude_globs,
        } => cmd_context(
            &service,
            &dir,
            diff.as_deref(),
            diff_file.as_deref(),
            git,
            &base,
            staged,
            pretty,
            &exclude_dirs,
            &exclude_globs,
        ),
        Commands::Impact {
            dir,
            git,
            base,
            staged,
            hook,
            exclude_dirs,
            exclude_globs,
        } => cmd_impact(
            &service,
            &dir,
            git,
            &base,
            staged,
            hook,
            &exclude_dirs,
            &exclude_globs,
        ),
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
            dead_scope,
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
            dead_scope,
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
