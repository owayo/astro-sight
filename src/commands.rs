use anyhow::Result;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::models::cochange::{CoChangeOptions, CoChangeResult};
use crate::models::skip::SkipInfo;
use crate::service::{AppService, AstParams};

mod common;

pub use crate::output::{OutputFormat, OutputOptions, serialize_cli_document, serialize_document};
#[cfg(test)]
pub(crate) use common::read_bytes_limited_and_drain;
pub(crate) use common::{ChangedFileSet, cache_hash_for_path, log_phase, read_to_string_limited};
pub use common::{MAX_INPUT_SIZE, classify_error, read_paths_file_limited};

// ---------------------------------------------------------------------------
// 単一ファイル系コマンド（キャッシュ・出力フォーマット対応）
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
    pub output: OutputOptions,
}

pub struct CmdSymbolsDirOpts<'a> {
    pub dir: &'a str,
    pub glob: Option<&'a str>,
    pub include_generated: bool,
    pub doc: bool,
    pub full: bool,
    pub query: Option<&'a str>,
    pub output: OutputOptions,
}

/// 単一ファイル系コマンド (ast / symbols) の共通キャッシュ機構。
/// read_file → content_hash → cache_hash_for_path → use_cache 判定 → get →
/// ヒット時 raw 書出 → miss 時 produce で serialize → TOCTOU ガード → put の流れを共通化する。
///
/// `produce` は cache miss 時のみ呼ばれ、`(改行付き serialize 済み出力, service が実際に解析した
/// 内容の hash)` を返す。ast と symbols で response の借用期間が違うため serialize は produce 側に
/// 閉じる。JSON compact 以外 (pretty / TOON) では get も put もしない
/// (`use_cache = !no_cache && output.cacheable()`)。cache 初期化・
/// get・put の失敗は解析失敗に昇格させず黙殺する。
fn run_cached_file_command<F>(
    command: &str,
    path: &str,
    no_cache: bool,
    output: OutputOptions,
    cache_key: &str,
    produce: F,
) -> Result<()>
where
    F: FnOnce() -> Result<(String, Option<String>)>,
{
    let utf8_path = camino::Utf8Path::new(path);
    let source = parser::read_file(utf8_path)?;
    let content_hash = CacheStore::hash(&source);
    let hash = cache_hash_for_path(utf8_path, &content_hash);
    let use_cache = !no_cache && output.cacheable();

    if use_cache
        && let Ok(cache) = CacheStore::new()
        && let Some(cached) = cache.get(&hash, cache_key)
    {
        info!(
            command = command,
            path = path,
            output_bytes = cached.len(),
            cached = true,
            "💾 cache hit"
        );
        std::io::Write::write_all(&mut std::io::stdout(), &cached)?;
        return Ok(());
    }

    let (text, analyzed_hash) = produce()?;

    info!(
        command = command,
        path = path,
        output_bytes = text.len(),
        "command completed"
    );

    // TOCTOU ガード: cache key の hash は最初の read の内容から計算しているが、service は
    // 同じファイルを再 read して解析する。2 回の read の間に更新が入ると hash(旧内容) →
    // output(新内容) の組で cache が汚染されるため、service が実際に解析した内容の hash
    // (analyzed_hash) が最初の read 内容と一致する場合のみ put する。
    if use_cache
        && analyzed_hash.as_deref() == Some(content_hash.as_str())
        && let Ok(cache) = CacheStore::new()
    {
        let _ = cache.put(&hash, cache_key, text.as_bytes());
    }

    print!("{text}");
    Ok(())
}

pub fn cmd_ast(service: &AppService, opts: &CmdAstOpts<'_>) -> Result<()> {
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

    run_cached_file_command(
        "ast",
        opts.path,
        opts.no_cache,
        opts.output,
        &cache_key,
        || {
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

            let output = if opts.full {
                serialize_cli_document(&response, opts.output)?
            } else {
                serialize_cli_document(&response.to_compact_ast(), opts.output)?
            };

            // response の借用期間が symbols と異なるため、解析済み hash はここで clone して返す。
            let analyzed_hash = response.hash.clone();
            Ok((output, analyzed_hash))
        },
    )
}

pub fn cmd_symbols_dir(service: &AppService, opts: &CmdSymbolsDirOpts<'_>) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(opts.dir)?;
    let collection = crate::engine::refs::collect_files_scan(
        &canonical_dir,
        opts.glob,
        crate::engine::refs::FileScanOptions {
            include_generated: opts.include_generated,
        },
    )?;
    let skipped = collection.skipped(&canonical_dir);
    let file_paths: Vec<String> = collection
        .files
        .iter()
        .filter_map(|p| p.to_str().map(|s| s.to_string()))
        .collect();
    batch_symbols(
        service,
        &file_paths,
        crate::commands::batch::BatchSymbolsOpts {
            doc: opts.doc,
            full: opts.full,
            dir: Some(&canonical_dir),
            skipped,
            query: opts.query,
            output: opts.output,
        },
    )
}

pub fn cmd_symbols(
    service: &AppService,
    path: &str,
    no_cache: bool,
    output: OutputOptions,
    doc: bool,
    full: bool,
    query: Option<&str>,
) -> Result<()> {
    // v3_: Symbol に enclosing container フィールド追加 (compact では `cn` キー)
    // custom query は結果集合が変わるため query hash を key へ混ぜる
    // (default query の cache key は従来のまま不変)。
    let base_key = if full {
        "v3_symbols_full"
    } else if doc {
        "v3_symbols_doc"
    } else {
        "v3_symbols"
    };
    let cache_key = match query {
        Some(q) => format!("{base_key}_q{}", &CacheStore::hash(q.as_bytes())[..16]),
        None => base_key.to_string(),
    };

    run_cached_file_command("symbols", path, no_cache, output, &cache_key, || {
        let response = service.extract_symbols_with_query(path, query)?;
        let analyzed_hash = response.hash.clone();

        let text = if full {
            serialize_cli_document(&response, output)?
        } else {
            let compact = response.to_compact_symbols(doc);
            serialize_cli_document(&compact, output)?
        };

        Ok((text, analyzed_hash))
    })
}

pub fn cmd_calls(
    service: &AppService,
    path: &str,
    function: Option<&str>,
    output: OutputOptions,
) -> Result<()> {
    let result = service.extract_calls(path, function)?;
    // `--pretty` は整形だけでなく full / compact DTO の選択も兼ねている (既存仕様)。
    // TOON には pretty の概念が無いため既定の compact DTO を使う。
    let text = if output.is_pretty_json() {
        serialize_cli_document(&result, output)?
    } else {
        serialize_cli_document(&result.to_compact(), output)?
    };
    info!(command = "calls", path = path, function = ?function, output_bytes = text.len(), "command completed");
    print!("{text}");
    Ok(())
}

pub fn cmd_imports(service: &AppService, path: &str, output: OutputOptions) -> Result<()> {
    let result = service.extract_imports(path)?;
    let text = serialize_cli_document(&result, output)?;
    info!(
        command = "imports",
        path = path,
        output_bytes = text.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

pub fn cmd_lint(
    service: &AppService,
    path: &str,
    rules: &[crate::models::lint::Rule],
    output: OutputOptions,
) -> Result<()> {
    let result = service.lint_file(path, rules)?;
    let text = serialize_cli_document(&result, output)?;
    info!(
        command = "lint",
        path = path,
        rules_count = rules.len(),
        output_bytes = text.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

pub fn cmd_sequence(
    service: &AppService,
    path: &str,
    function: Option<&str>,
    output: OutputOptions,
) -> Result<()> {
    let result = service.generate_sequence(path, function)?;
    let text = serialize_cli_document(&result, output)?;
    info!(command = "sequence", path = path, function = ?function, output_bytes = text.len(), "command completed");
    print!("{text}");
    Ok(())
}

pub fn cmd_refs(
    service: &AppService,
    name: &str,
    dir: &str,
    glob: Option<&str>,
    include_generated: bool,
    output: OutputOptions,
) -> Result<()> {
    let result = service.find_references_with_generated(name, dir, glob, include_generated)?;
    let text = serialize_cli_document(&result, output)?;
    info!(command = "refs", name = name, dir = dir, glob = ?glob, output_bytes = text.len(), "command completed");
    print!("{text}");
    Ok(())
}

pub fn cmd_refs_batch(
    service: &AppService,
    names: &[String],
    dir: &str,
    glob: Option<&str>,
    include_generated: bool,
    output: OutputOptions,
) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut total_refs = 0usize;

    // find_references_batch が内部で名前を chunk 分割しつつディレクトリ走査を 1 回に
    // 集約するため、ここでは全名を 1 回で渡す（以前は呼び出し側で chunk 分割していたが
    // chunk 毎に walk し直していた）。service は入力順を保った `Vec<RefsResult>` を返すので
    // NDJSON 出力も names 順を維持する。
    let (results, skipped) =
        service.find_references_batch_with_generated(names, dir, glob, include_generated)?;
    for result in &results {
        total_refs += result.references.len();
    }

    // この経路は元々全件を `Vec` で保持しているため、TOON では NDJSON ではなく
    // 1 個のルート配列ドキュメントとして出す (ストリーミング要件が無いので、
    // tabular 判定まで効く canonical なエンコードが使える)。
    // auto も全件を持っているぶん近似が要らず、NDJSON 全体と TOON ドキュメントを
    // 実際に組み立てて短い方を選べる。
    match output.format() {
        OutputFormat::Json => {
            for result in &results {
                let line = serde_json::to_string(result)?;
                writeln!(out, "{line}")?;
            }
            if let Some(skipped) = &skipped {
                writeln!(out, "{}", serde_json::json!({ "skipped": skipped }))?;
            }
        }
        OutputFormat::Toon => {
            if let Some(skipped) = &skipped {
                let mut records: Vec<serde_json::Value> = results
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<_, _>>()?;
                records.push(serde_json::json!({ "skipped": skipped }));
                write!(out, "{}", serialize_document(&records, output)?)?;
            } else {
                write!(out, "{}", serialize_document(&results, output)?)?;
            }
        }
        OutputFormat::Auto => {
            let mut ndjson = String::new();
            for result in &results {
                ndjson.push_str(&serde_json::to_string(result)?);
                ndjson.push('\n');
            }
            if let Some(skipped) = &skipped {
                ndjson.push_str(&serde_json::to_string(
                    &serde_json::json!({ "skipped": skipped }),
                )?);
                ndjson.push('\n');
            }
            let toon = if let Some(skipped) = &skipped {
                let mut records: Vec<serde_json::Value> = results
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<_, _>>()?;
                records.push(serde_json::json!({ "skipped": skipped }));
                serialize_document(&records, output.with_format(OutputFormat::Toon))?
            } else {
                serialize_document(&results, output.with_format(OutputFormat::Toon))?
            };
            if crate::output::estimated_size(&toon) < crate::output::estimated_size(&ndjson) {
                write!(out, "{toon}")?;
            } else {
                write!(out, "{ndjson}")?;
            }
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
    output: OutputOptions,
    skipped: Option<SkipInfo>,
) -> Result<()> {
    if let Some(skip) = skipped {
        // git 管理外 (起点ファイル無し): 空の entries + skipped を返して exit 0。
        let result = CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
            // 解析自体を行っていないので診断は空 (出力からも省略される)。
            diagnostics: Default::default(),
            skipped: Some(skip),
        };
        let text = serialize_cli_document(&result, output)?;
        print!("{text}");
        return Ok(());
    }
    let result = service.analyze_cochange(dir, opts)?;
    let text = serialize_cli_document(&result, output)?;
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
        output_bytes = text.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

mod git_input;

pub use git_input::{
    BlameSourceResolution, DEFAULT_BLAME_BASE, resolve_blame_source_files, run_git_diff,
};
pub(crate) use git_input::{DiffSourceResolution, resolve_diff_source};
// GitDiffInput / resolve_git_diff は Task 4 で resolve_diff_source に内包され、
// 非テストコードからの直接参照は無くなった。tests.rs のみが `super::*` 経由で使う。
#[cfg(test)]
pub(crate) use git_input::git_show_blob;
#[cfg(test)]
pub(crate) use git_input::{
    GitDiffInput, is_git_work_tree, resolve_git_diff, validate_git_revision,
};

/// `cmd_context` の引数一式。`diff`/`diff_file` と `git`/`staged` のような
/// 隣接同型引数の取り違えを型と名前で防ぐ (`CmdAstOpts` / `CmdReviewOpts` と同じ流儀)。
pub struct CmdContextOpts<'a> {
    pub dir: &'a str,
    pub diff: Option<&'a str>,
    pub diff_file: Option<&'a str>,
    pub git: bool,
    pub base: &'a str,
    pub staged: bool,
    pub output: OutputOptions,
    pub exclude_dirs: &'a [String],
    pub exclude_globs: &'a [String],
}

pub fn cmd_context(service: &AppService, opts: &CmdContextOpts<'_>) -> Result<()> {
    // 本体は従来の局所変数名のまま使うためここで一括分解する (全フィールド Copy)。
    let &CmdContextOpts {
        dir,
        diff,
        diff_file,
        git,
        base,
        staged,
        output,
        exclude_dirs,
        exclude_globs,
    } = opts;
    let (diff_input, truncations) =
        match resolve_diff_source(dir, diff, diff_file, git, base, staged)? {
            DiffSourceResolution::Diff { diff, truncations } => (diff, truncations),
            DiffSourceResolution::Skipped(skip) => {
                // git 管理外: 空の changes + skipped を返して exit 0。
                let result = crate::models::impact::ContextResult {
                    changes: Vec::new(),
                    skipped: Some(skip),
                    truncations: Vec::new(),
                };
                print!("{}", serialize_cli_document(&result, output)?);
                return Ok(());
            }
            DiffSourceResolution::NotRequested => {
                let stdin = std::io::stdin();
                (
                    read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?,
                    Vec::new(),
                )
            }
        };

    let options = crate::models::impact::ContextAnalysisOptions {
        exclude_dirs: exclude_dirs.to_vec(),
        exclude_globs: exclude_globs.to_vec(),
    };

    // 逐次出力できるのは compact JSON だけ。pretty は整形が要り、TOON はルート配列の
    // 要素数 `[N]` が解析完了まで確定せず、auto は両形式を比べ終えるまで勝者が決まらない。
    // いずれも全 FileImpact を集約してから一括 serialize する。数 GB 級リポでは
    // 既定の compact JSON (下の streaming 経路) を使うこと。
    if !output.streams_compact_json() {
        let mut result = service.analyze_context(&diff_input, dir, &options)?;
        result.truncations = truncations;
        let text = serialize_cli_document(&result, output)?;
        info!(
            command = "context",
            dir = dir,
            diff_bytes = diff_input.len(),
            output_bytes = text.len(),
            "command completed"
        );
        print!("{text}");
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
    out.write_all(b"]")?;
    // 打ち切りがあれば streaming 出力にも載せる (No silent caps)。空なら従来どおり省略。
    if !truncations.is_empty() {
        out.write_all(b",\"truncations\":")?;
        serde_json::to_writer(&mut out, &truncations)
            .map_err(|e| anyhow::anyhow!("json serialization failed: {e}"))?;
    }
    out.write_all(b"}\n")?;
    info!(
        command = "context",
        dir = dir,
        diff_bytes = diff_input.len(),
        changes = changes_count,
        truncations = truncations.len(),
        "command completed (streaming)"
    );
    Ok(())
}

/// `cmd_impact` の引数一式。隣接する `bool` 3 連 (`git`/`staged`/`hook`) の取り違えを
/// 型と名前で防ぐ (`CmdAstOpts` / `CmdReviewOpts` と同じ流儀)。
pub struct CmdImpactOpts<'a> {
    pub dir: &'a str,
    pub git: bool,
    pub base: &'a str,
    pub staged: bool,
    pub hook: bool,
    pub output: OutputOptions,
    pub exclude_dirs: &'a [String],
    pub exclude_globs: &'a [String],
}

pub fn cmd_impact(service: &AppService, opts: &CmdImpactOpts<'_>) -> Result<()> {
    // 本体は従来の局所変数名のまま使うためここで一括分解する (全フィールド Copy)。
    let &CmdImpactOpts {
        dir,
        git,
        base,
        staged,
        hook,
        output,
        exclude_dirs,
        exclude_globs,
    } = opts;
    // impact は構造化 JSON 出力を持たず、`--hook` 出力は Stop hook の JSON 契約。
    // 明示的な `--format toon` は満たせないのでここで弾く (config 由来なら JSON に倒す)。
    output.ensure_json_protocol("impact")?;
    // impact に inline `--diff` / `--diff-file` は無いため resolver へは None を渡す。
    let (diff_input, truncations) = match resolve_diff_source(dir, None, None, git, base, staged)? {
        DiffSourceResolution::Diff { diff, truncations } => (diff, truncations),
        // git 管理外: 既存の「差分なし」と同じく無出力で exit 0。
        // impact は構造化 JSON 出力を持たず未解決 caller 検出時のみ stderr に
        // 出力する設計のため、skipped JSON は出さない (hook の有無を問わず silent)。
        DiffSourceResolution::Skipped(_) => return Ok(()),
        DiffSourceResolution::NotRequested => {
            let stdin = std::io::stdin();
            (
                read_to_string_limited(stdin.lock(), MAX_INPUT_SIZE, "stdin input")?,
                Vec::new(),
            )
        }
    };
    // impact は構造化 JSON を持たないため、打ち切りは人間向け stderr 行として出す
    // (解析範囲が欠けたことを黙って落とさない)。hook 経路でも同じ行が出る。
    for t in &truncations {
        eprintln!(
            "note: {} ({})",
            t.message,
            t.path.as_deref().unwrap_or("(global)")
        );
    }

    if diff_input.trim().is_empty() {
        return Ok(());
    }

    let options = crate::models::impact::ContextAnalysisOptions {
        exclude_dirs: exclude_dirs.to_vec(),
        exclude_globs: exclude_globs.to_vec(),
    };
    let result = service.analyze_context(&diff_input, dir, &options)?;

    // 変更されたファイルパスを事前に canonicalize してキャッシュ（O(M) syscall に削減）
    let changed = ChangedFileSet::build(dir, result.changes.iter().map(|c| c.path.as_str()));

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
            // caller のファイルが変更ファイルに含まれていないか（= diff 内で未解決か）を判定
            if !changed.contains_caller(dir, &caller.path) {
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
        // 内部の line は tree-sitter 由来の 0-indexed。人間向け表示 (path:line) だけ
        // エディタと同じ 1-indexed に補正する (JSON 出力の基準は変えない)。
        for caller in callers {
            if caller.symbols.is_empty() {
                eprintln!("  → {}:{}", caller.path, caller.line + 1);
            } else {
                eprintln!(
                    "  → {}:{} [{}]",
                    caller.path,
                    caller.line + 1,
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

pub fn cmd_doctor(output: OutputOptions) -> Result<()> {
    let report = doctor::run_doctor();
    let text = serialize_cli_document(&report, output)?;
    info!(
        command = "doctor",
        output_bytes = text.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

pub fn cmd_session(output: OutputOptions) -> Result<()> {
    // session は「1 行 = 1 リクエスト / 1 レスポンス」の NDJSON プロトコル。
    // 複数行になる TOON は載せられないため JSON 固定。
    output.ensure_json_protocol("session")?;
    let service = AppService::from_env()?;
    crate::session::run_session(|req| handle_request(&service, req))
}

pub fn cmd_mcp(output: OutputOptions) -> Result<()> {
    use rmcp::ServiceExt;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // JSON-RPC transport 自体は常に JSON。`output` はツール結果の
        // text content をどちらで返すかだけを決める。
        let server = crate::mcp::AstroSightServer::new(output);
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

mod review;

#[cfg(test)]
pub(crate) use review::hook::build_review_hook_json;
pub use review::{CmdReviewOpts, cmd_review};

mod api_changes;
mod dead_code;
mod dead_code_member_liveness;

#[cfg(test)]
pub(crate) use api_changes::*;
pub use dead_code::{CmdDeadCodeOpts, cmd_dead_code};
// tests.rs が `use super::*` 経由で参照する dead_code 内部シンボル。
// (production 側の唯一の利用者だった cmd_review は review モジュールへ移動し、
//  そこから `super::dead_code::…` を直接参照している)
#[cfg(test)]
pub(crate) use dead_code::{
    auto_detect_framework, detect_dead_symbols_from_files, extract_dead_code_candidates_from_file,
    filter_dead_by_wip_added, filter_diff_files_for_dead_code, resolve_dead_code_excludes,
    resolve_framework_globs_with_auto_detect,
};

mod batch;
mod session_handler;

pub use batch::{
    BatchSymbolsOpts, batch_ast, batch_calls, batch_imports, batch_lint, batch_sequence,
    batch_symbols,
};
pub use session_handler::handle_request;

#[cfg(test)]
mod tests;
