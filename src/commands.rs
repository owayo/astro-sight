use anyhow::{Result, anyhow, bail};
use rayon::prelude::*;
use std::collections::HashSet;
use std::io::Read;
use tracing::info;

use crate::cache::store::CacheStore;
use crate::doctor;
use crate::engine::parser;
use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{CoChangeOptions, CoChangeResult};
use crate::models::dead_code::DeadCodeResult;
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, DeadSymbol, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
use crate::models::skip::SkipInfo;
use crate::service::{AppService, AstParams};

// ---------------------------------------------------------------------------
// 共通ヘルパー
// ---------------------------------------------------------------------------

pub const MAX_INPUT_SIZE: usize = 100 * 1024 * 1024;

/// 現在プロセスの RSS を KB 単位で取得 (Linux のみ正確、その他 OS は None)。
/// `astro-sight review` の各フェーズが何 GB 消費しているかを CI の artifacts ログで
/// 観測するため。
pub(crate) fn current_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(kb) = parts.first().and_then(|s| s.parse::<u64>().ok()) {
                    return Some(kb);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `ASTRO_SIGHT_LOG_PHASES=1` のときのみ stderr に進捗ログを出す。
///
/// CI で `astro-sight review` がどのフェーズで何 GB を確保するかを観測するための
/// 軽量プロファイラ。出力フォーマットは:
/// `[as] phase=<NAME> status=<start|end> rss=<MB> elapsed=<MS>`
pub(crate) fn log_phase(phase: &str, status: &str, elapsed_ms: u128) {
    if std::env::var("ASTRO_SIGHT_LOG_PHASES").ok().as_deref() != Some("1") {
        return;
    }
    let rss_str = current_rss_kb()
        .map(|kb| format!("{}MB", kb / 1024))
        .unwrap_or_else(|| "?MB".to_string());
    eprintln!("[as] phase={phase} status={status} rss={rss_str} elapsed={elapsed_ms}ms");
}

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

pub fn read_paths_file_limited(path: &str, max_bytes: usize) -> Result<Vec<String>> {
    let content = match read_file_to_string_limited(path, max_bytes) {
        Ok(content) => content,
        Err(e) if e.downcast_ref::<AstroError>().is_some() => return Err(e),
        Err(e) => {
            return Err(AstroError::new(
                ErrorCode::IoError,
                format!("failed to read paths file {path}: {e}"),
            )
            .into());
        }
    };

    Ok(content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

fn cache_hash_for_path(path: &camino::Utf8Path, source: &[u8]) -> String {
    let content_hash = CacheStore::hash(source);
    let path_key = std::fs::canonicalize(path.as_std_path())
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| path.as_str().to_string());

    // 応答には path/lang が含まれるため、内容が同じ別ファイルとはキャッシュを分離する。
    CacheStore::hash(format!("{path_key}\0{content_hash}").as_bytes())
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

/// blame モード用の起点ファイル解決。
/// 優先順位: --paths-file > --paths > --git。複数指定時は明示の方を採用 (--git は追加扱い)。
/// いずれも空なら InvalidRequest エラー。
///
/// `user_exclude_globs` は `BLAME_DEFAULT_EXCLUDE_GLOBS` と合わせ、`--git` 経由で
/// diff から自動収集した起点ファイルに対してのみ適用する。`--paths` / `--paths-file`
/// で明示指定された起点はユーザー意図を尊重してフィルタしない (lock ファイル等を
/// 意図的に分析したいケースを想定)。
/// `resolve_blame_source_files` の結果。起点ファイルが解決できたか、
/// git 管理外 (かつ明示 `--paths` / `--paths-file` 無し) で skip かを型で表す。
pub enum BlameSourceResolution {
    Files(Vec<String>),
    Skipped(SkipInfo),
}

pub fn resolve_blame_source_files(
    dir: &str,
    git: bool,
    base: Option<&str>,
    paths: Option<&str>,
    paths_file: Option<&str>,
    user_exclude_globs: &[String],
) -> Result<BlameSourceResolution> {
    use std::collections::BTreeSet;

    let mut set: BTreeSet<String> = BTreeSet::new();

    if let Some(file_path) = paths_file {
        for path in read_paths_file_limited(file_path, MAX_INPUT_SIZE)? {
            set.insert(path);
        }
    }
    if let Some(s) = paths {
        for p in s.split(',') {
            let p = p.trim();
            if !p.is_empty() {
                set.insert(p.to_string());
            }
        }
    }
    if git {
        // git 管理外: 明示 --paths/--paths-file があればそれで続行、
        // 無ければ graceful skip (既存の明示優先の優先順位を維持)。
        if !is_git_work_tree(dir)? {
            if set.is_empty() {
                return Ok(BlameSourceResolution::Skipped(
                    SkipInfo::not_git_repository(),
                ));
            }
            return Ok(BlameSourceResolution::Files(set.into_iter().collect()));
        }
        let base_rev = base.unwrap_or("HEAD~1");
        validate_git_revision(base_rev, "base")?;
        let output = std::process::Command::new("git")
            .args(["diff", "--name-only", base_rev, "HEAD"])
            .current_dir(dir)
            .output()
            .map_err(|e| {
                anyhow::Error::from(crate::error::AstroError::new(
                    crate::error::ErrorCode::IoError,
                    format!("failed to run git diff: {e}"),
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(crate::error::AstroError::new(
                crate::error::ErrorCode::IoError,
                format!("git diff failed: {stderr}"),
            ));
        }
        // `--git` 経由は自動収集なので生成物・ロック類を除外する。
        // 明示指定の `--paths` / `--paths-file` には適用しない。
        let matcher = crate::engine::cochange::CoChangeExclude::build(user_exclude_globs)?;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let p = line.trim();
            if p.is_empty() {
                continue;
            }
            if matcher.is_match(p) {
                continue;
            }
            set.insert(p.to_string());
        }
    }

    if set.is_empty() {
        anyhow::bail!(crate::error::AstroError::new(
            crate::error::ErrorCode::InvalidRequest,
            "blame mode requires source files: pass --git, --paths, or --paths-file".to_string(),
        ));
    }
    Ok(BlameSourceResolution::Files(set.into_iter().collect()))
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

/// `--git` 入力解決の結果。diff が取れたか、git 管理外で skip かを型で表す。
enum GitDiffInput {
    Diff(String),
    Skipped(SkipInfo),
}

/// `dir` が git worktree 内かを `git rev-parse --is-inside-work-tree` で判定する。
///
/// 管理外 (`not a git repository`) / worktree 外 (`is-inside-work-tree=false`) は
/// `Ok(false)` (skip 対象)、壊れた repo / 権限不足 / git 実行不能などの本物の異常は
/// `Err` (従来どおり `exit 1`)。`git diff` の stderr 解析ではなく事前判定にすることで
/// worktree / submodule / bare repo に堅牢。`LC_ALL=C` で stderr 文言のロケール依存を
/// 排除し `"not a git repository"` のマッチを安定させる。
fn is_git_work_tree(dir: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim() == "true");
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("not a git repository") {
        return Ok(false);
    }

    // 壊れた repo / 権限など本物の異常は従来どおりエラー (fail-closed)。
    Err(AstroError::new(
        ErrorCode::InvalidRequest,
        format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )
    .into())
}

/// 経路A (context / impact / review / dead-code) の git diff 入力解決。
///
/// git worktree 内なら diff を取得し `Diff`、管理外なら `Skipped` を返す。
/// `base` 検証を worktree 判定より前に置くのは意図的: base が不正なら git 管理外
/// でも `exit 1` にする (入力契約違反を skip より優先)。
fn resolve_git_diff(dir: &str, base: &str, staged: bool) -> Result<GitDiffInput> {
    validate_git_revision(base, "--base")?;

    if !is_git_work_tree(dir)? {
        return Ok(GitDiffInput::Skipped(SkipInfo::not_git_repository()));
    }

    run_git_diff(dir, base, staged).map(GitDiffInput::Diff)
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

/// `--hook` の出力判定結果。
/// - `value`: stderr に書き出す JSON (何もなければ None)
/// - `is_blocking`: exit 1 にして Stop hook を止めるべきか。cochange だけは informational
///   として block しない (レポート 2026-04-11-cochange-new-repo-initial-commit-noise.md の提案)
struct HookJsonBuild {
    value: Option<serde_json::Value>,
    is_blocking: bool,
}

fn build_review_hook_json(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
) -> HookJsonBuild {
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
        // hook の `syms` には「実際に cross-file caller を発生させた causal シンボル」だけを残す。
        // `change.affected_symbols` を丸ごと入れると、is_symbol_exported で cross-file 検索を
        // 弾かれた非 export const や、隣接 hunk の context に巻き込まれた未変更 export まで
        // hook 出力に混ざる。`caller.symbols` (cross-file 検索を通過した causal name) と
        // `affected_symbols` の交差で causal だけを抽出する。
        //
        // また `change_type == "added"` のシンボルは「同コミットで新規追加され、まだ既存
        // 呼び出し側を持っていない export」。hook (stop blocking 判定) では「新規依存関係」
        // として価値はあるが、breaking change ではないため除外する。通常 `review` の
        // `impact.changes[].impacted_callers` には引き続き残る (情報価値を維持)。
        // (Issue: 2026-05-27-added-symbol-initial-reference)
        let affected_change_types: std::collections::HashMap<&str, &str> = change
            .affected_symbols
            .iter()
            .map(|s| (s.name.as_str(), s.change_type.as_str()))
            .collect();
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
                // breaking causal シンボル (modified / removed) だけを残す。
                // `added` 由来は hook blocking から除外する。
                let causal_syms: Vec<String> = caller
                    .symbols
                    .iter()
                    .filter(|sym| {
                        matches!(
                            affected_change_types.get(sym.as_str()).copied(),
                            Some(ct) if ct != "added"
                        )
                    })
                    .cloned()
                    .collect();
                if causal_syms.is_empty() {
                    // 全 caller.symbols が added 由来 (または affected 外) → blocking しない
                    continue;
                }
                let entry = unresolved.entry(change.path.clone()).or_default();
                for sym in &causal_syms {
                    entry.changed_symbols.insert(sym.clone());
                }
                entry
                    .refs
                    .push((caller.path.clone(), caller.line, causal_syms));
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

    // api: {add,rm,mod,moved,property_to_field,rm_dead,const_value} — 空でないセクションのみ
    let has_api_changes = !result.api_changes.added.is_empty()
        || !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty()
        || !result.api_changes.moved.is_empty()
        || !result.api_changes.property_to_field.is_empty()
        || !result.api_changes.removed_dead.is_empty()
        || !result.api_changes.modified_closed_in_diff.is_empty()
        || !result.api_changes.const_value_changes.is_empty()
        || !result.api_changes.compatible_modified.is_empty();
    // api.added / api.moved / api.property_to_field / api.removed_dead / api.const_value は
    // 破壊的変更ではないため Stop hook のブロッキング対象から外し informational 扱いにする。
    // api.removed / api.modified は破壊的変更の可能性があるため従来どおり blocking。
    // const_value (値のみ変更) は `--strict-public-const-values` 指定時のみ blocking に昇格する。
    let has_api_breaking = !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty()
        || (strict_const_values && !result.api_changes.const_value_changes.is_empty());
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
        // mod_closed: 全 cross-file 参照が同一 diff 内で追随済みの api.mod。informational
        // (has_api_breaking に含めないため stop hook をブロックしない)。
        if !result.api_changes.modified_closed_in_diff.is_empty() {
            api.insert(
                "mod_closed".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .modified_closed_in_diff
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        // const_value: const / 非 mut static / export const の値のみ変更。shape (名前・型・
        // visibility) は不変でコンパイル互換性を壊さないため informational
        // (デフォルト非 blocking、`--strict-public-const-values` 指定時のみ blocking 昇格)。
        if !result.api_changes.const_value_changes.is_empty() {
            api.insert(
                "const_value".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .const_value_changes
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        // mod_compat: signature 文字列は変わったが公開契約が維持される互換 api.mod
        // (React HOC ラップ / 未参照プロパティ削除)。informational (非 blocking)。reason 付き。
        if !result.api_changes.compatible_modified.is_empty() {
            api.insert(
                "mod_compat".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .compatible_modified
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file, "reason": m.reason}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.moved.is_empty() {
            api.insert(
                "moved".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .moved
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "n": m.name,
                                "from": m.from,
                                "to": m.to,
                            })
                        })
                        .collect(),
                ),
            );
        }
        if !result.api_changes.removed_dead.is_empty() {
            api.insert(
                "rm_dead".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .removed_dead
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.property_to_field.is_empty() {
            api.insert(
                "property_to_field".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .property_to_field
                        .iter()
                        .map(|p| serde_json::json!({"n": p.name, "f": p.file}))
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
fn review_hook_output(result: &ReviewResult, dir: &str, strict_const_values: bool) -> Result<()> {
    let build = build_review_hook_json(result, dir, strict_const_values);
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
                // bin-only crate (src/lib.rs なし) の pub シンボル、および private module
                // 配下の pub シンボルは crate 外から構造的に到達できないため api.add 対象外。
                let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path)
                    || is_rust_symbol_in_private_module(dir, &df.new_path);
                // 新規ファイルでも、同一ファイル内で呼ばれている関数は内部ヘルパーと
                // 判断して api.add から除外する。CLI スクリプト (main から内部関数を
                // 呼び出す構造) を新規追加した時に全関数が api.add に積まれるノイズを防ぐ。
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
                // Rust bin-only crate (`[[bin]]` のみで `[lib]` なし) の `pub fn` は
                // クレート外から到達できないため、削除されても外部 API の破壊にはならない。
                // `api.add` 側 (line 1623, 1724) と対称に `api.rm` 側でも除外する。
                // `api.rm` は旧 API 面の判定なので、`base` リビジョン時点での crate type
                // を見る (新ツリーで src/lib.rs を削除したケースで誤抑制しないため)。
                let is_binary_rust_old_crate =
                    is_binary_only_rust_crate_at_base(dir, base, &df.old_path);
                for (name, kind, sig) in &old_syms {
                    if is_binary_rust_old_crate {
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

        // bin-only crate (src/lib.rs なし) の pub シンボル、および private module 配下の
        // pub シンボルは crate 外から構造的に到達できないため api.add の対象外とする。
        let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path)
            || is_rust_symbol_in_private_module(dir, &df.new_path);

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
        // Rust bin-only crate (`[[bin]]` のみで `[lib]` なし) の `pub fn` は外部から
        // 到達できないため、削除されても破壊的 API 変更にはならない。`api.add` 側と対称に
        // 除外する (Issue 2026-05-19-api-rm-bin-crate-dead-cleanup 対応)。
        // `api.rm` は旧 API 面の判定なので、`base` リビジョン時点の crate type を見る。
        let is_binary_rust_old_crate = is_binary_only_rust_crate_at_base(dir, base, &df.old_path);
        for (name, kind, sig) in &old_syms {
            if !new_map.contains_key(name.as_str()) {
                if is_binary_rust_old_crate {
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
        // api.mod の private module 抑制: new と base 両方で private module 配下なら crate 外
        // 非到達なので除外する。base で外部公開 (pub mod) だった旧 API の破壊的 signature 変更は
        // base 側が public 判定となり除外されず blocking を維持する (codex 指摘2)。
        let skip_mod_for_binary_crate = is_binary_rust_old_crate_for_mod
            || is_binary_rust_new_crate_for_mod
            || (is_rust_symbol_in_private_module(dir, &df.new_path)
                && is_rust_symbol_in_private_module_at_base(dir, base, &df.old_path));

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
                    name,
                    &df.new_path,
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
fn detect_react_wrapper_compatible_mod(
    dir: &str,
    name: &str,
    file: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    if !matches!(
        lang_id,
        Some(LangId::Typescript | LangId::Tsx | LangId::Javascript)
    ) {
        return None;
    }
    // new 側が memo / forwardRef でラップされていること (単なる function 本体変更は対象外)。
    if !new_sig_has_react_wrapper(new_sig) {
        return None;
    }
    // old / new 双方の内側 function 引数リストを抽出して正規化比較する。
    let old_params = extract_function_param_list(old_sig)?;
    let new_params = extract_function_param_list(new_sig)?;
    if old_params != new_params {
        return None;
    }
    // 型注釈が無い (props だけ等) と JSX 互換を保証できないため blocking 維持。
    if !old_params.contains(':') {
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
        file: file.to_string(),
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

/// signature から `function <name>(<params>)` の `<params>` を抽出し whitespace 正規化して
/// 返す。memo/forwardRef ラップでも内側 `function` 宣言を見る。`function` キーワードが無い
/// (arrow 等) / paren 対応が取れない場合は None (保守的に blocking)。
fn extract_function_param_list(sig: &str) -> Option<String> {
    let bytes = sig.as_bytes();
    let kw = b"function";
    let mut i = 0;
    let mut fn_after = None;
    while i + kw.len() <= bytes.len() {
        if &bytes[i..i + kw.len()] == kw {
            let before_ok = i == 0 || {
                let p = bytes[i - 1];
                !(p.is_ascii_alphanumeric() || p == b'_' || p == b'$')
            };
            let after = i + kw.len();
            let after_ok = bytes
                .get(after)
                .is_some_and(|&a| a == b' ' || a == b'\t' || a == b'*' || a == b'(');
            if before_ok && after_ok {
                fn_after = Some(after);
                break;
            }
        }
        i += 1;
    }
    let start = fn_after?;
    let open = bytes[start..].iter().position(|&b| b == b'(')? + start;
    let close = find_matching_paren_bytes(bytes, open)?;
    let params = std::str::from_utf8(bytes.get(open + 1..close)?).ok()?;
    Some(params.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `bytes[open]` が `(` のとき対応する `)` の index を返す。文字列リテラルとネストを考慮。
fn find_matching_paren_bytes(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => in_str = Some(b),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
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
                let last_word = ctx[..i].split_whitespace().next_back().unwrap_or("");
                let is_typeof = last_word == "typeof";
                let is_new = last_word == "new";
                if is_call || is_member || is_typeof || is_new {
                    return false;
                }
                let is_jsx = before == Some(b'<') || (i >= 2 && &bytes[i - 2..i] == b"</");
                if !is_jsx {
                    // JSX でも値利用でもない裸の出現は判定不能 → 安全側 (blocking)
                    return false;
                }
            }
        }
        i += 1;
    }
    saw_occurrence
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

/// Rust シンボルのファイルが crate root (src/lib.rs) から `pub mod` 経路で到達できない
/// private module 配下にあるかを判定する。private module 配下のシンボルは crate 外から
/// 構造的に到達できず外部公開 API ではないため、api 差分 (add/mod) の対象外とする
/// (Issue 2026-05-29-swift-sidecar-api-mod パターンC)。
///
/// 軽量実装: lib.rs から mod 宣言チェーンを辿り、各セグメントが制限なし `pub mod` かを
/// 確認する。`pub(crate)` / `pub(super)` 等の制限付き pub や `mod` (private) は外部非到達。
/// `#[path]` 属性 / inline mod / 宣言未検出など解析できないケースは false を返して保守的に
/// API 扱い (既存挙動維持) とする。bin-only crate (lib.rs なし) も false。
fn rust_symbol_in_private_module_inner(dir: &str, file_path: &str, base: Option<&str>) -> bool {
    use std::path::{Path, PathBuf};
    let rel = Path::new(file_path);
    if rel.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
        return false;
    };
    let abs = canonical_dir.join(rel);
    // crate root: abs の祖先で最も近い Cargo.toml を持つディレクトリ (canonical_dir 境界まで)。
    // ディレクトリ構造は working tree のものを使う (base で大きく変わるケースは稀)。
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
    let Some(crate_root) = crate_root else {
        return false;
    };
    let src_dir = crate_root.join("src");
    if !src_dir.join("lib.rs").is_file() {
        return false; // library crate でない (bin-only は別経路で処理)
    }
    let Ok(rel_to_src) = abs.strip_prefix(&src_dir) else {
        return false; // src/ 配下でないファイル
    };
    let segments = module_path_segments(rel_to_src);
    if segments.is_empty() {
        return false; // root モジュール (lib.rs / main.rs) は常に到達可能
    }
    let crate_root_rel = crate_root.strip_prefix(&canonical_dir).ok();
    // src 相対モジュールパス → source。working tree (base=None) は直接読み、base 指定時は
    // `git show <base>:<crate>/src/<rel>` で旧版を取得する (codex 指摘2: base で外部公開
    // だった旧 API を誤抑制しないよう old 側でも可視性を確認するため)。
    let read_module = |module_rel: &Path| -> Option<Vec<u8>> {
        match base {
            None => std::fs::read(src_dir.join(module_rel)).ok(),
            Some(base) => {
                let crate_rel = crate_root_rel?;
                let full_rel = crate_rel.join("src").join(module_rel);
                let full_rel_str = full_rel.to_str()?;
                if validate_git_revision(base, "--base").is_err()
                    || validate_git_revision(full_rel_str, "diff file path").is_err()
                {
                    return None;
                }
                let out = std::process::Command::new("git")
                    .args(["show", &format!("{base}:{full_rel_str}")])
                    .current_dir(dir)
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                Some(out.stdout)
            }
        }
    };
    // lib.rs から mod 宣言チェーンを辿り、いずれかが非 pub なら private 配下。
    // 次モジュールファイルの存在確認は working tree 構造で代用する。
    let mut current_rel = PathBuf::from("lib.rs");
    for seg in &segments {
        let Some(source) = read_module(&current_rel) else {
            return false;
        };
        let Ok(tree) = parser::parse_source(&source, crate::language::LangId::Rust) else {
            return false;
        };
        match find_mod_decl_visibility(tree.root_node(), &source, seg) {
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
                    return false; // モジュールファイル解決不能 → 保守的に API 扱い
                }
            }
            Some(false) => {
                // private mod でも `pub use <seg>::...` re-export されていれば外部到達可能。
                if find_pub_use_reexport(tree.root_node(), &source, seg) {
                    return false;
                }
                return true; // private mod 配下 → 外部非到達
            }
            None => return false, // mod 宣言が見つからない → 保守的に API 扱い
        }
    }
    false
}

/// working tree のシンボルが private module 配下 (crate 外非到達) かを判定する。
fn is_rust_symbol_in_private_module(dir: &str, file_path: &str) -> bool {
    rust_symbol_in_private_module_inner(dir, file_path, None)
}

/// base リビジョン時点でシンボルが private module 配下だったかを判定する (git show 経由)。
fn is_rust_symbol_in_private_module_at_base(dir: &str, base: &str, file_path: &str) -> bool {
    rust_symbol_in_private_module_inner(dir, file_path, Some(base))
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
            let mut mc = child.walk();
            let is_pub = child.children(&mut mc).any(|c| {
                c.kind() == "visibility_modifier" && c.utf8_text(source).map(str::trim) == Ok("pub")
            });
            return Some(is_pub);
        }
    }
    None
}

/// AST を走査し `pub use <mod_name>::...` の re-export を探す (制限なし pub のみ対象)。
fn find_pub_use_reexport(node: tree_sitter::Node<'_>, source: &[u8], mod_name: &str) -> bool {
    if node.kind() == "use_declaration"
        && let Ok(text) = node.utf8_text(source)
    {
        let t = text.trim_start();
        // `pub use foo::...` の re-export。`pub(crate) use` 等の制限付きは外部非公開なので除外。
        if t.starts_with("pub use") && t.contains(&format!("{mod_name}::")) {
            return true;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if find_pub_use_reexport(child, source, mod_name) {
            return true;
        }
    }
    false
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
fn bare_name(qualname: &str) -> &str {
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
pub(crate) fn detect_dead_symbols_from_files(
    dir: &str,
    files: &[std::path::PathBuf],
) -> (Vec<DeadSymbol>, Vec<DeadSymbol>) {
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    // case-insensitive 言語 (Xojo 等) のみで構成された files では dead-code 検出を
    // skip する。
    //
    // v26.5 まで: CI 言語 (Xojo) は tree-sitter parse が OOM する問題で diff 全体を skip。
    // v26.6 以降: tree-sitter-xojo を削除し lexer-only に移行。dead-code は lexer 経由で
    // 動作するため CI skip 機構は不要。`ASTRO_SIGHT_FORCE_CI_LANG_DEAD_CODE` は deprecate
    // (no-op、警告も出さない)。

    // .gitattributes の linguist-generated 指定ファイルは dead-code 検出から除外する
    let gitattrs = crate::engine::gitattributes::GitAttributes::load(&canonical_dir);

    // 全ファイルのエクスポートシンボルを収集（trait impl メソッドは除外）
    // (original_name, kind, file, lang_id) — case-insensitive 言語では lang_id で
    // シンボル名を正規化した比較を行うため lang も保持する。
    let mut all_syms: Vec<(String, String, String, crate::language::LangId)> = Vec::new();
    // C/C++ の追加 liveness 情報 (file, シンボル名, 追加名リスト, lang)。
    // enum→列挙子名 / typedef tag→alias 名。後で正規化して liveness_aliases に変換する。
    let mut liveness_raw: Vec<(String, String, Vec<String>, crate::language::LangId)> = Vec::new();
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
        // C/C++ では enum の列挙子・typedef alias を liveness 補助名として集める。
        // enum 型名が直接使われなくても列挙子が使われていれば live、body あり typedef tag が
        // alias 名でのみ使われていても live と判定するために使う (Issue #11/#12)。
        if matches!(
            lang,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) {
            for (sym, extras) in collect_cpp_liveness_for_file(dir, &rel, lang) {
                liveness_raw.push((rel.clone(), sym, extras, lang));
            }
        }
    }

    if all_syms.is_empty() {
        return (Vec::new(), Vec::new());
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

    // C/C++ の (file, 正規化シンボル名) → 追加 liveness 名 (正規化済み) を構築。
    // enum 候補は列挙子名、typedef tag 候補は alias 名を介した参照でも live と判定する。
    let mut liveness_aliases: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (file, sym, extras, lang) in &liveness_raw {
        let key = norm_bare(*lang, sym);
        let extra_keys: Vec<String> = extras.iter().map(|e| norm_bare(*lang, e)).collect();
        liveness_aliases
            .entry((file.clone(), key))
            .or_default()
            .extend(extra_keys);
    }

    // 全シンボル名の非 Definition 参照件数をカウント（SymbolReference を確保しない）。
    // 入力も正規化済みキーで渡し、refs 側の HashMap キーと lookup を一致させる。
    // liveness 補助名 (列挙子 / alias) も検索対象に含め、enum/tag の生存判定に使う。
    let unique_names: Vec<String> = {
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for (name, _, _, lang) in &all_syms {
            let k = norm_bare(*lang, name);
            if seen.insert(k.clone()) {
                names.push(k);
            }
        }
        for extras in liveness_aliases.values() {
            for ek in extras {
                if seen.insert(ek.clone()) {
                    names.push(ek.clone());
                }
            }
        }
        names
    };

    // production / test 別に refs カウント。test/ 配下のみで参照されるシンボルは
    // dead_symbols ではなく test_only_symbols として分離する (F5)。
    let counts = match crate::engine::refs::count_non_definition_refs_split(
        &unique_names,
        &canonical_dir,
        None,
        is_test_path,
    ) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    // Android プロジェクトでは `AndroidManifest.xml` / layout XML から
    // シンボルが参照されうる（`<activity android:name=".MainActivity"/>` 等）。
    // Kotlin/Java AST のみでは追跡できない Android framework 経由の生存判定を補うため、
    // XML 参照集合に含まれるシンボルは dead から除外する。
    // AndroidManifest.xml が存在しないプロジェクトでは空集合が返り副作用なし。
    let xml_refs = crate::engine::xml_refs::collect_xml_symbol_references(&canonical_dir);

    // Angular プロジェクトでは `*.component.html` テンプレートや
    // `@Component({ template: \`...\` })` の inline template 内の binding 式から
    // component method/プロパティが参照される。TypeScript AST のみでは追跡できない
    // ため、テンプレート参照集合に含まれるシンボルは dead から除外する。
    // angular.json / *.component.ts のどちらも見つからないプロジェクトでは空集合が
    // 返り副作用なし。
    let template_refs =
        crate::engine::angular_template_refs::collect_angular_template_refs(&canonical_dir);

    // production 0 / test 0 → dead_symbols
    // production 0 / test > 0 → test_only_symbols (F5)
    // production > 0 → 生存とみなしどちらにも報告しない
    let mut dead = Vec::new();
    let mut test_only = Vec::new();
    for (name, kind, file, lang) in &all_syms {
        let key = norm_bare(*lang, name);
        // 同名シンボルが複数存在する場合は bare name では区別できないためスキップ
        if name_counts.get(&key).copied().unwrap_or(0) > 1 {
            continue;
        }

        let (mut prod_cnt, mut test_cnt) = counts.get(&key).copied().unwrap_or((0, 0));
        // C/C++ の enum 列挙子 / typedef alias 経由の参照も合算する。enum 型名が直接
        // 使われなくても列挙子が使われていれば live、body あり typedef tag が alias 名でのみ
        // 使われていても live と判定する (Issue #11/#12)。
        if let Some(extra_keys) = liveness_aliases.get(&(file.clone(), key.clone())) {
            for ek in extra_keys {
                if let Some((p, t)) = counts.get(ek) {
                    prod_cnt += p;
                    test_cnt += t;
                }
            }
        }
        if prod_cnt > 0 {
            continue;
        }

        // bare name と qualname (Container.method) の両方を XML 参照と突き合わせる。
        // layout XML の `android:onClick="handler"` は単純名でしか書けないため bare で検索し、
        // `android:name=".Foo"` 等で Container 側をカバーするケースは qualname でも検査する。
        let bare = bare_name(name);
        if xml_refs.contains(bare) || xml_refs.contains(name.as_str()) {
            continue;
        }

        // Angular template 参照 (`(event)="handler()"` 等) は bare name でのみ
        // 出現するため bare で突き合わせる。`Container.method` 形式の qualname も
        // 念のため両方確認する。
        if template_refs.contains(bare) || template_refs.contains(name.as_str()) {
            continue;
        }

        let sym = DeadSymbol {
            name: name.clone(),
            kind: kind.clone(),
            file: file.clone(),
        };
        if test_cnt > 0 {
            // PHPUnit テストクラス内のメソッドが test 配下からのみ参照されている場合は
            // 同一クラス内の self::/static::/$this-> ヘルパー、または @dataProvider /
            // @depends / #[DataProvider] 経由で reflection 呼び出しされる helper である
            // 可能性が高く、test_only_symbols としてレポートしてもユーザーには
            // 「テストランナーが内部で使うだけのノイズ」になる。container 名が PHPUnit
            // テストクラス規約に合致するメソッドは test_only からも除外する。
            if matches!(*lang, crate::language::LangId::Php)
                && let Some((container, _)) = name.rsplit_once('.')
            {
                let container_short = container
                    .rsplit_once('.')
                    .map(|(_, t)| t)
                    .unwrap_or(container);
                if is_phpunit_test_class_name(container_short) {
                    continue;
                }
            }
            test_only.push(sym);
        } else {
            dead.push(sym);
        }
    }

    (dead, test_only)
}

/// C/C++ ファイルをパースし、dead-code liveness 補助情報 (enum→列挙子 / typedef tag→alias) を返す。
/// `detect_dead_symbols_from_files` で enum / typedef tag の生存判定を補強するために使う。
fn collect_cpp_liveness_for_file(
    dir: &str,
    rel: &str,
    lang: crate::language::LangId,
) -> Vec<(String, Vec<String>)> {
    let full = std::path::Path::new(dir).join(rel);
    let Some(full_str) = full.to_str() else {
        return Vec::new();
    };
    let utf8 = camino::Utf8Path::new(full_str);
    let Ok(source) = parser::read_file(utf8) else {
        return Vec::new();
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return Vec::new();
    };
    crate::engine::symbols::collect_cpp_dead_liveness_aliases(tree.root_node(), &source, lang)
}

/// テストディレクトリとみなすセグメント名一覧。
///
/// - 言語共通: `tests`, `Tests`, `__tests__`, `spec`, `testdata`
/// - JVM/Gradle 標準: `test` (`src/test/`), `androidTest`, `sharedTest`, `integrationTest`
///
/// `is_test_path` (API 差分検出) と `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` (dead-code 既定除外)
/// の両側で同じ判定を行うため一元化する。`is_test_path` が `test` 単数形を含む一方で
/// `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` には含まれない、という履歴的なねじれ
/// (2026-05-21 の JUnit Kotlin dead 誤検出として顕在化) を解消する。
const TEST_DIRECTORY_SEGMENTS: &[&str] = &[
    "tests",
    "test",
    "Tests",
    "__tests__",
    "spec",
    "testdata",
    "androidTest",
    "sharedTest",
    "integrationTest",
];

/// refs カウントを production / test に振り分けるための判定関数。
///
/// - ファイル名規約 (`*_test.go`, `*Test.php`, `*_spec.rb` 等) は既存の
///   `is_test_file_path` に委譲する。
/// - ディレクトリセグメント規約は `TEST_DIRECTORY_SEGMENTS` に一元化。
fn is_test_path(path: &std::path::Path) -> bool {
    if let Some(s) = path.to_str() {
        if crate::engine::impact::test_context::is_test_file_path(s) {
            return true;
        }
        if s.split('/')
            .any(|seg| TEST_DIRECTORY_SEGMENTS.contains(&seg))
        {
            return true;
        }
    }
    false
}

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

fn extract_dead_code_candidates_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // dead-code 走査では既定でテストディレクトリ (tests/, Tests/, __tests__/, spec/,
    // testdata/) が collect 段階で除外される。`--include-tests` で opt-in したときは
    // テストファイルも走査対象に含めるため、ここでは test_path 除外を行わない
    // (API 検出側 extract_exported_symbols_from_file は test path 除外を行う)。
    //
    // dead-code 判定では Typer / Click / FastAPI / Flask / pytest 等のフレームワーク
    // 登録デコレータが付いた関数を除外する。デコレータ経由でフレームワーク内部
    // レジストリに登録されるため、識別子レベルの cross-file refs では caller を
    // 追跡できず偽陽性源になる。
    extract_exported_symbols_from_file_inner(dir, file_path, true, true)
}

fn extract_exported_symbols_from_file_inner(
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
            &source, lexer_lang,
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

/// dead_symbols のうち、宣言行が今回の diff の追加行 (`+` 行) と重なるもののみを残す。
///
/// `--dead-scope touched-symbols` の実装。`review --hook` のデフォルトとして使われ、
/// 「changed file 内に元からあった dead」がレビューノイズとして毎回出る UX 問題を
/// 解消する。
///
/// 注意: `HunkInfo` の `new_start` / `new_count` は context 行も含むため
/// hunk 範囲全体を「touched」と扱うと既存 dead まで残してしまう。ここでは
/// `extract_changed_new_lines` で **実際に追加された行** だけを set 化して照合する。
fn filter_dead_by_touched_symbols(
    dir: &str,
    dead: Vec<crate::models::review::DeadSymbol>,
    diff_input: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> Vec<crate::models::review::DeadSymbol> {
    use std::collections::{HashMap, HashSet};

    // changed file 集合 (削除ファイルは含めない)。
    let mut changed_files: HashSet<&str> = HashSet::new();
    for df in diff_files {
        if df.new_path != "/dev/null" {
            changed_files.insert(df.new_path.as_str());
        }
    }

    // 「ファイル → 追加行 set (0-indexed)」「ファイル → シンボル名→宣言行」を per-file キャッシュ。
    let mut changed_lines_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut sym_lines_cache: HashMap<String, HashMap<String, usize>> = HashMap::new();

    dead.into_iter()
        .filter(|ds| {
            if !changed_files.contains(ds.file.as_str()) {
                // diff に含まれないファイル: touched ではないので除外。
                return false;
            }
            let changed_lines = changed_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| {
                    crate::engine::diff::extract_changed_new_lines(diff_input, &ds.file)
                });
            let line_map = sym_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| extract_symbol_lines(dir, &ds.file).unwrap_or_default());
            let Some(&line) = line_map.get(&ds.name) else {
                // 宣言行が引けない (lexer-only で取り漏れ等) は保守的に touched 扱いで残す。
                return true;
            };
            changed_lines.contains(&line)
        })
        .collect()
}

/// ファイル内シンボルの「名前 → 宣言行 (0-indexed)」マップを返す。
fn extract_symbol_lines(
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

    // bare_name -> (def_count, ref_count)
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for r in &batch_result {
        let mut def_count = 0usize;
        let mut ref_count = 0usize;
        for x in &r.references {
            if x.kind == Some(RefKind::Definition) {
                def_count += 1;
            } else {
                ref_count += 1;
            }
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
/// - `tests`, `Tests`, `__tests__`, `spec`, `testdata`,
///   `test`, `androidTest`, `sharedTest`, `integrationTest`: 言語共通 + JVM/Gradle のテストディレクトリ
///   (実体は `TEST_DIRECTORY_SEGMENTS` 定数で `is_test_path` と共有)
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
/// dead-code 既定除外のテストディレクトリ。`is_test_path` と同じセグメント集合
/// (`TEST_DIRECTORY_SEGMENTS`) を使い、API 検出側と dead-code 側のテスト判定を統一する。
const DEFAULT_DEAD_CODE_EXCLUDES_TESTS: &[&str] = TEST_DIRECTORY_SEGMENTS;
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

/// Laravel 規約プリセット。フレームワークが自動で呼び出す規約的エントリポイントを除外する。
///
/// - `database/migrations/**`: Artisan `migrate` から `up()` / `down()` を呼ぶ
/// - `database/seeds/**` / `database/seeders/**` / `database/factories/**`: Artisan `db:seed` が `run()` を呼ぶ
/// - `database/views/**`: DB view 定義 (Artisan 駆動)
/// - `app/Console/Commands/**`: `handle()` が Artisan から呼ばれる
/// - `app/Http/Controllers/**`: Route 定義 (`routes/web.php` 等) から文字列経由で呼ばれる
/// - `app/Http/Middleware/**`: `handle()` が Route/Kernel 経由で呼ばれる
/// - `app/Http/Requests/**`: `authorize()` / `rules()` が Form Request 解決時に自動呼出し
/// - `app/Http/Resources/**`: `toArray()` が Response serialization で呼ばれる
/// - `app/GraphQL/**`: GraphQL schema ファイルから文字列経由で解決される
/// - `app/Listeners/**`, `app/Providers/**`: Service Container / Event Bus 経由
/// - `_ide_helper*.php`, `.phpstorm.meta.php`: IDE 補助の自動生成ファイル
///
/// `**/` 接頭辞でサブディレクトリに埋め込まれた Laravel アプリ（モノレポ内の複数 Laravel
/// 等）にも対応する。
const LARAVEL_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // 標準マイグレーション経路 (Artisan 駆動)
    "**/database/migrations/**",
    // Multi-DB / 複数コネクション構成で派生する migrations_foo, migrations-foo
    // (Laravel 公式 ドキュメントの `--path` 指定パターン) も同様に Artisan 駆動
    "**/database/migrations_*/**",
    "**/database/migrations-*/**",
    // シーダー / ファクトリ / ビュー定義 / テーブル定義スナップショット
    "**/database/seeds/**",
    "**/database/seeders/**",
    "**/database/factories/**",
    "**/database/views/**",
    "**/database/TableDefinitions/**",
    // Artisan / Route / GraphQL 経由で呼ばれるエントリポイント
    "**/app/Console/Commands/**",
    "**/app/Http/Controllers/**",
    "**/app/Http/Middleware/**",
    "**/app/Http/Requests/**",
    "**/app/Http/Resources/**",
    "**/app/GraphQL/**",
    "**/app/Listeners/**",
    "**/app/Providers/**",
    // bootstrap/app.php で ExceptionHandler 規約で登録されるハンドラ
    "**/app/Exceptions/**",
    // Service Container / Observer / Cast / Policy / Event / Queue / Mail / Notification /
    // Broadcast channel / FormRequest validation Rule — いずれも Laravel のフレームワーク側が
    // reflection / 文字列 FQN / 自動ディスパッチで呼び出す規約的エントリポイント群
    "**/app/Casts/**",
    "**/app/Observers/**",
    "**/app/Policies/**",
    "**/app/Events/**",
    "**/app/Jobs/**",
    "**/app/Notifications/**",
    "**/app/Mail/**",
    "**/app/Rules/**",
    "**/app/Broadcasting/**",
    // IDE 補助の自動生成ファイル
    "**/_ide_helper.php",
    "**/_ide_helper_models.php",
    "**/.phpstorm.meta.php",
];

/// Next.js (App Router / Pages Router) のフレームワーク entrypoint プリセット。
///
/// Next.js のファイルシステムルーティングでは、特定のファイル名 (`page` / `layout` /
/// `route` 等) の default export が Next.js ランタイム経由で呼ばれる。AST 上の
/// cross-file refs では caller を追跡できないため、astro-sight 単独では
/// `dead-code` の偽陽性源になる。`--framework nextjs` でこれらを除外する。
///
/// - **App Router** (Next.js 13+): `app/**/page.*`, `layout.*`, `loading.*`, `error.*`,
///   `not-found.*`, `template.*`, `default.*`, `global-error.*`, `route.*`
/// - **Pages Router** (legacy): `pages/**/*.{js,jsx,ts,tsx}` (含む `pages/api/**`)
/// - **Root entrypoints**: `middleware.{js,ts}`, `instrumentation.{js,ts}`
///
/// `src/app/**` のような src layout もそのまま `**/app/**` のグロブでカバーされる。
const NEXTJS_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // App Router 規約ファイル
    "**/app/**/page.{js,jsx,ts,tsx}",
    "**/app/**/layout.{js,jsx,ts,tsx}",
    "**/app/**/loading.{js,jsx,ts,tsx}",
    "**/app/**/error.{js,jsx,ts,tsx}",
    "**/app/**/not-found.{js,jsx,ts,tsx}",
    "**/app/**/template.{js,jsx,ts,tsx}",
    "**/app/**/default.{js,jsx,ts,tsx}",
    "**/app/**/global-error.{js,jsx,ts,tsx}",
    "**/app/**/route.{js,jsx,ts,tsx}",
    // Pages Router (legacy)
    "**/pages/**/*.{js,jsx,ts,tsx}",
    // Root entrypoints
    "**/middleware.{js,ts}",
    "**/instrumentation.{js,ts}",
];

/// `resolve_framework_globs` の auto-detect 対応版。
///
/// 呼び出し側で `framework` が明示指定されていれば従来通り `resolve_framework_globs` に
/// 委譲する。未指定の場合は `dir` 直下の `package.json` を読んで `next` 依存を検出し、
/// 見つかれば `"nextjs"` プリセットを適用する。明示指定が auto detect より常に優先される。
///
/// 自動検出に失敗した場合 (package.json なし、JSON パース失敗、依存不一致) は空 Vec を
/// 返す。debug ログを出さない (副作用最小化のため、検出結果は呼び出し側の review JSON 等で
/// 表現する余地を残す)。
fn resolve_framework_globs_with_auto_detect(
    framework: Option<&str>,
    dir: &str,
) -> Result<Vec<String>> {
    if framework.is_some() {
        return resolve_framework_globs(framework);
    }
    match auto_detect_framework(dir) {
        Some(name) => resolve_framework_globs(Some(name)),
        None => Ok(Vec::new()),
    }
}

/// `dir/package.json` を読んで Next.js プロジェクトかを判定する。
///
/// 判定: `package.json` の `dependencies` または `devDependencies` に `next` キーが存在
/// すること。`peerDependencies` / `optionalDependencies` は Next.js ライブラリやテスト
/// fixture で誤爆しやすいため対象外。
///
/// 失敗時 (ファイル無し / JSON パース失敗 / next 依存なし) は `None` を返す。
///
/// モノレポでの workspace 走査は将来対応 (初期実装は root `package.json` のみ)。
fn auto_detect_framework(dir: &str) -> Option<&'static str> {
    let pkg_path = std::path::Path::new(dir).join("package.json");
    let text = std::fs::read_to_string(&pkg_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let has_next = ["dependencies", "devDependencies"].iter().any(|field| {
        value
            .get(field)
            .and_then(|v| v.as_object())
            .is_some_and(|deps| deps.contains_key("next"))
    });
    if has_next { Some("nextjs") } else { None }
}

/// フレームワーク名から対応する除外 glob プリセットを返す。
/// 未知のフレームワーク名はエラー。
///
/// `**/app/X/**` / `**/database/X/**` のような app-prefix 付きパターンには、
/// `**/X/**` という prefix 省略版も自動で追加する。これにより以下が同時にカバーされる:
/// - `--dir <project>/app` のように `app/` 直下を指した場合の fallback
/// - `app/` を別名 (例: `core/`) にリネームしている独自レイアウト
/// - Laravel 配下に複数 module を抱えるモノレポ (`<root>/<sub>/Http/Controllers/...`)
///
/// 過剰除外の懸念: `**/Http/**` の類は Laravel 規約以外でも使われ得るが、
/// 既定除外に `vendor/` / `node_modules/` 等のサードパーティ配下が入っており、
/// なおかつ `--framework laravel` を指定しているのは Laravel プロジェクトのみという
/// 前提なので、実用上の誤マッチはほぼ発生しない。
fn resolve_framework_globs(framework: Option<&str>) -> Result<Vec<String>> {
    match framework {
        None => Ok(Vec::new()),
        Some(name) => match name.to_ascii_lowercase().as_str() {
            "laravel" => {
                let mut globs: Vec<String> =
                    Vec::with_capacity(LARAVEL_PRESET_EXCLUDE_GLOBS.len() * 2);
                for pat in LARAVEL_PRESET_EXCLUDE_GLOBS {
                    globs.push((*pat).to_string());
                    // app/database prefix の省略版を並列で登録 (--dir が app/ 直下の場合の fallback、
                    // および Laravel 標準外レイアウトへの自動対応)
                    if let Some(rest) = pat
                        .strip_prefix("**/app/")
                        .or_else(|| pat.strip_prefix("**/database/"))
                    {
                        globs.push(format!("**/{rest}"));
                    }
                }
                Ok(globs)
            }
            "nextjs" | "next" => {
                // Next.js は `app/` と `pages/` が予約ディレクトリ名で、`src/app/`
                // / `src/pages/` レイアウトも `**/app/**` / `**/pages/**` グロブで
                // そのままカバーされるため prefix 省略形は不要。
                // むしろ `**/pages/**/*.{js,jsx,ts,tsx}` の省略形は
                // `**/*.{js,jsx,ts,tsx}` となり全 TS/JS ファイルを誤除外するので
                // Laravel と異なり省略形を生成しない。
                Ok(NEXTJS_PRESET_EXCLUDE_GLOBS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect())
            }
            other => Err(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Unknown framework preset: {other} (supported: laravel, nextjs)"),
            )
            .into()),
        },
    }
}

/// 指定パスが既定除外対象のディレクトリセグメントを含むかを判定する。
fn path_is_default_excluded(path: &str, excludes: &[&str]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    path.split('/').any(|seg| excludes.contains(&seg))
}

/// `diff_files` を dead-code 検出対象に絞り込む共通ヘルパー。
/// `cmd_dead_code` と `cmd_review` の両者から呼び、除外ロジックを一元化する。
///
/// - `excludes`: 既定除外ディレクトリ名 (vendor / tests / build 等、呼び出し側で合成済み)
/// - `combined_exclude_globs`: framework プリセット + ユーザ指定 `--exclude-glob` を合成したパターン列
/// - `glob`: positive glob フィルタ。指定時は whitelist されたもののみ残す。
pub(crate) fn filter_diff_files_for_dead_code(
    canonical_dir: &std::path::Path,
    diff_files: &[crate::models::impact::DiffFile],
    excludes: &[&str],
    combined_exclude_globs: &[&str],
    glob: Option<&str>,
) -> Result<Vec<std::path::PathBuf>> {
    // 除外判定は workspace 相対の new_path で行う。canonical_dir に `test` 等の
    // 親セグメントが含まれているケース (例: `/private/tmp/test/myrepo`) でも、
    // リポ内の `src/foo.rs` のような non-test ファイルを誤って除外しないようにするため。
    let mut files: Vec<std::path::PathBuf> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        // diff の new_path は信頼境界外。絶対パスやトラバーサル成分を含むパスは
        // canonical_dir.join() で workspace 外を指してしまうため、ここで弾く。
        .filter(|f| crate::engine::impact::is_safe_diff_path(&f.new_path))
        .filter(|f| !path_is_default_excluded(&f.new_path, excludes))
        .map(|f| canonical_dir.join(&f.new_path))
        .filter(|p| {
            crate::language::LangId::from_path(camino::Utf8Path::new(p.to_str().unwrap_or("")))
                .is_ok()
        })
        .collect();

    if glob.is_some() || !combined_exclude_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(canonical_dir);
        if let Some(pattern) = glob {
            ob.add(pattern)?;
        } else {
            ob.add("**/*")?;
        }
        for pat in combined_exclude_globs {
            let negated = if pat.starts_with('!') {
                (*pat).to_string()
            } else {
                format!("!{pat}")
            };
            ob.add(&negated)?;
        }
        let overrides = ob.build()?;
        files.retain(|p| !overrides.matched(p, false).is_ignore());
        // glob が指定されているときは「whitelist に明示マッチ」だけを残す。
        // `Match::None` (どのパターンにもマッチしない) を許可すると、
        // `--glob '**/*.py'` のような絞り込みでも Rust ファイル等が残ってしまう。
        if glob.is_some() {
            files.retain(|p| overrides.matched(p, false).is_whitelist());
        }
    }
    Ok(files)
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
        SymbolKind::Class => is_phpunit_test_class_name(short),
        SymbolKind::Method | SymbolKind::Function => {
            matches!(
                short,
                "setUp" | "tearDown" | "setUpBeforeClass" | "tearDownAfterClass"
            ) || is_phpunit_test_method_name(short)
        }
        _ => false,
    }
}

/// PHPUnit テストクラス名規約 (`*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest`)
/// に合致するか判定する。test_only_symbols 振り分け時に container 名と突き合わせる。
fn is_phpunit_test_class_name(name: &str) -> bool {
    name.ends_with("Test")
        || name.ends_with("TestCase")
        || name.ends_with("IntegrationTest")
        || name.ends_with("FeatureTest")
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

/// `unittest.TestCase` / `unittest.IsolatedAsyncioTestCase` を直接示す base 名集合。
const PYTHON_UNITTEST_ROOT_BASES: &[&str] = &[
    "TestCase",
    "unittest.TestCase",
    "IsolatedAsyncioTestCase",
    "unittest.IsolatedAsyncioTestCase",
];

/// 同一ファイル内の Python クラスについて、`unittest.TestCase` 系を直接/間接継承する
/// クラス名集合を fixed-point で解決して返す。クロスファイル継承は対象外。
///
/// 例: `class Base(unittest.TestCase): ...` と `class Child(Base): ...` の両方を拾う。
fn collect_python_unittest_classes(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
) -> std::collections::HashSet<String> {
    use crate::models::symbol::SymbolKind;
    let mut unittest_classes: std::collections::HashSet<String> = std::collections::HashSet::new();
    if lang_id != crate::language::LangId::Python {
        return unittest_classes;
    }

    // (クラス名, 解決待ち base 名のリスト) のペアを集める。
    let mut class_bases: Vec<(String, Vec<String>)> = Vec::new();
    for sym in syms {
        if !matches!(sym.kind, SymbolKind::Class) {
            continue;
        }
        let bases = crate::engine::symbols::python_class_base_names(root, source, &sym.range);
        // 直接 root base を継承していれば即座に確定。
        if bases
            .iter()
            .any(|b| PYTHON_UNITTEST_ROOT_BASES.contains(&b.as_str()))
        {
            unittest_classes.insert(sym.name.clone());
            continue;
        }
        // それ以外は候補として保留し、後段で fixed-point 解決する。
        class_bases.push((sym.name.clone(), bases));
    }

    // 同一ファイル内の Base → Child チェーンを fixed-point で広げる。
    loop {
        let mut changed = false;
        let mut idx = 0;
        while idx < class_bases.len() {
            let inherited = class_bases[idx]
                .1
                .iter()
                .any(|b| unittest_classes.contains(b.as_str()));
            if inherited {
                let (name, _) = class_bases.swap_remove(idx);
                unittest_classes.insert(name);
                changed = true;
            } else {
                idx += 1;
            }
        }
        if !changed {
            break;
        }
    }

    unittest_classes
}

/// ファイル名が pytest のモジュール命名規約 (`test_*.py` または `*_test.py`) に
/// 一致するかを判定する。`conftest.py` は別関数で判定する。
fn file_name_is_pytest_module(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !file_name.ends_with(".py") {
        return false;
    }
    // conftest.py は別ハンドリング。
    if file_name == "conftest.py" {
        return false;
    }
    if file_name.starts_with("test_") {
        return true;
    }
    // `*_test.py` 規約 (ファイル名が `_test.py` で終わる)。
    let stem = file_name.trim_end_matches(".py");
    stem.ends_with("_test") && stem.len() > "_test".len()
}

/// ファイル名が pytest の `conftest.py` かどうかを判定する。
fn file_name_is_python_conftest(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("conftest.py")
}

/// `unittest` / pytest のテスト規約に該当する Python シンボルかを判定する。
///
/// 対象:
/// - `unittest.TestCase` 直接/間接継承クラス (同一ファイル内のチェーン)
/// - そのクラス配下の `test_*` メソッドおよび `setUp` / `tearDown` /
///   `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup`
/// - `test_*.py` / `*_test.py` のトップレベル `test_*` 関数 (pytest 規約)
/// - `conftest.py` 内のすべての関数 (pytest フィクスチャ規約)
fn is_python_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
    file_path: Option<&str>,
    container: Option<&str>,
    unittest_classes: &std::collections::HashSet<String>,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Python {
        return false;
    }

    // qualname (`Foo.test_bar`) の場合は末尾要素を取り出して container を補正する。
    let (short, qual_container) = match name.rsplit_once('.') {
        Some((head, tail)) => (tail, Some(head)),
        None => (name, None),
    };
    let effective_container = container.or(qual_container);

    if matches!(kind, SymbolKind::Class) {
        return unittest_classes.contains(short);
    }

    if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        return false;
    }

    // conftest.py 内の関数はすべて pytest 規約で参照されうる。
    if file_name_is_python_conftest(file_path) && effective_container.is_none() {
        return true;
    }

    // `test_*.py` / `*_test.py` のトップレベル `test_*` 関数は pytest が discover する。
    if file_name_is_pytest_module(file_path)
        && effective_container.is_none()
        && short.starts_with("test_")
    {
        return true;
    }

    // unittest.TestCase 派生クラス配下のメソッド。
    if let Some(class_name) = effective_container
        && unittest_classes.contains(class_name)
    {
        return short.starts_with("test_")
            || matches!(
                short,
                "setUp"
                    | "tearDown"
                    | "setUpClass"
                    | "tearDownClass"
                    | "asyncSetUp"
                    | "asyncTearDown"
                    | "addCleanup"
                    | "addClassCleanup"
                    | "addAsyncCleanup"
            );
    }

    false
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
    framework: Option<&str>,
    extra_exclude_dirs: &[String],
    extra_exclude_globs: &[String],
    pretty: bool,
    dead_scope: crate::cli::DeadScope,
) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    if !canonical_dir.is_dir() {
        return Err(
            AstroError::new(ErrorCode::InvalidRequest, format!("Not a directory: {dir}")).into(),
        );
    }

    let default_excludes = resolve_dead_code_excludes(include_vendor, include_tests, include_build);
    let mut excludes: Vec<&str> = default_excludes.to_vec();
    for name in extra_exclude_dirs {
        excludes.push(name.as_str());
    }

    // glob 除外: フレームワークプリセット + ユーザ指定
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    let mut combined_globs: Vec<&str> = framework_globs.iter().map(String::as_str).collect();
    for pat in extra_exclude_globs {
        combined_globs.push(pat.as_str());
    }

    // diff 指定があれば diff 関連ファイルのみ、なければプロジェクト全体
    let has_diff = diff.is_some() || diff_file.is_some() || git;
    // diff_input / diff_files は touched-symbols filter でも使うため、ここで一度だけ
    // 取得・parse して再利用する (旧実装は run_git_diff + parse_unified_diff を 2 回呼んでおり、
    // --staged 実行中の git add で 2 つの diff が乖離する競合状態があった)。
    let (diff_input, diff_files): (Option<String>, Option<Vec<crate::models::impact::DiffFile>>) =
        if has_diff {
            let input = if let Some(d) = diff {
                d.to_string()
            } else if let Some(df) = diff_file {
                read_file_to_string_limited(df, MAX_INPUT_SIZE)?
            } else {
                // git 経路 (diff/diff_file なし + has_diff): 管理外なら
                // 空の dead_symbols + skipped で exit 0。
                match resolve_git_diff(dir, base, staged)? {
                    GitDiffInput::Diff(s) => s,
                    GitDiffInput::Skipped(skip) => {
                        let result = DeadCodeResult {
                            dir: canonical_dir.to_string_lossy().to_string(),
                            scanned_files: 0,
                            dead_symbols: Vec::new(),
                            test_only_symbols: Vec::new(),
                            skipped: Some(skip),
                        };
                        let output = serialize_output(&result, pretty)?;
                        println!("{output}");
                        return Ok(());
                    }
                }
            };

            if input.trim().is_empty() {
                let result = DeadCodeResult {
                    dir: canonical_dir.to_string_lossy().to_string(),
                    scanned_files: 0,
                    dead_symbols: Vec::new(),
                    test_only_symbols: Vec::new(),
                    skipped: None,
                };
                let output = serialize_output(&result, pretty)?;
                println!("{output}");
                return Ok(());
            }

            let parsed = crate::engine::diff::parse_unified_diff(&input);
            (Some(input), Some(parsed))
        } else {
            (None, None)
        };

    let files: Vec<std::path::PathBuf> = if let Some(diff_files) = diff_files.as_ref() {
        filter_diff_files_for_dead_code(
            &canonical_dir,
            diff_files,
            &excludes,
            &combined_globs,
            glob,
        )?
    } else {
        crate::engine::refs::collect_files_with_excludes(
            &canonical_dir,
            glob,
            &excludes,
            &combined_globs,
        )?
    };

    let scanned_files = files.len();
    let (dead_symbols, test_only_symbols) = detect_dead_symbols_from_files(dir, &files);

    // dead-scope=touched-symbols: --git/--diff 指定時のみ意味を持つ。
    // diff の追加行情報が必要なので、has_diff のときだけ適用する。
    let dead_symbols = if matches!(dead_scope, crate::cli::DeadScope::TouchedSymbols)
        && let (Some(diff_input), Some(diff_files)) = (diff_input.as_deref(), diff_files.as_ref())
    {
        filter_dead_by_touched_symbols(dir, dead_symbols, diff_input, diff_files)
    } else {
        dead_symbols
    };

    let result = DeadCodeResult {
        dir: canonical_dir.to_string_lossy().to_string(),
        scanned_files,
        dead_symbols,
        test_only_symbols,
        skipped: None,
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
            let options = crate::models::impact::ContextAnalysisOptions {
                exclude_dirs: req.exclude_dirs.clone(),
                exclude_globs: req.exclude_globs.clone(),
            };
            let result = service.analyze_context(diff_input, dir, &options)?;
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
            let source_files = req.source_files.clone().unwrap_or_default();
            if source_files.is_empty() {
                bail!(crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    "cochange (blame mode) requires source_files".to_string(),
                ));
            }
            let opts = CoChangeOptions {
                source_files,
                base: req.base.clone(),
                min_confidence: req.min_confidence.unwrap_or(defaults.min_confidence),
                min_samples: req.min_samples.unwrap_or(defaults.min_samples),
                max_files_per_commit: req
                    .max_files_per_commit
                    .unwrap_or(defaults.max_files_per_commit),
                ..defaults
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
    fn read_paths_file_limited_trims_blank_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paths.txt");
        fs::write(&path, " src/main.rs \n\nCargo.toml\n").expect("write paths file");

        let paths =
            read_paths_file_limited(path.to_str().expect("utf-8 path"), 1024).expect("read paths");

        assert_eq!(paths, vec!["src/main.rs", "Cargo.toml"]);
    }

    #[test]
    fn read_paths_file_limited_rejects_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paths.txt");
        fs::write(&path, "abcde").expect("write paths file");

        let err = read_paths_file_limited(path.to_str().expect("utf-8 path"), 4)
            .expect_err("oversized paths-file should fail");

        assert!(err.to_string().contains("exceeds maximum size"));
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

    /// 複数行 grouped use ブロックの継続行で import されたシンボルの signature 変更でも、
    /// 呼び出し側を同一 diff で更新済みなら modified_closed_in_diff (informational) に
    /// 降格される。grouped use 継続行 (`    a, changed_fn, b,`) を未更新 caller と誤判定して
    /// blocking しないことを保証する (api.mod 誤検出 2026-05-31 の回帰防止)。
    #[test]
    fn detect_api_changes_modified_with_multiline_use_import_is_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // 旧: changed_fn を複数行 grouped use で import し呼び出す caller。
        git_commit_files(
            repo,
            &[
                ("src/target.rs", "pub fn changed_fn() -> i32 {\n    1\n}\n"),
                (
                    "src/caller.rs",
                    "use crate::target::{\n    changed_fn,\n    other_helper,\n};\n\npub fn other_helper() {}\n\npub fn run() {\n    let _ = changed_fn();\n}\n",
                ),
            ],
            "initial",
        );

        // 新: changed_fn の signature 変更 + 呼び出し更新。grouped use 行は不変。
        let src_dir = repo.join("src");
        fs::write(
            src_dir.join("target.rs"),
            "pub fn changed_fn(x: i32) -> i32 {\n    x\n}\n",
        )
        .expect("write new target");
        fs::write(
            src_dir.join("caller.rs"),
            "use crate::target::{\n    changed_fn,\n    other_helper,\n};\n\npub fn other_helper() {}\n\npub fn run() {\n    let _ = changed_fn(1);\n}\n",
        )
        .expect("write new caller");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/target.rs".to_string(),
                new_path: "src/target.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 3,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "src/caller.rs".to_string(),
                new_path: "src/caller.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 9,
                    old_count: 1,
                    new_start: 9,
                    new_count: 1,
                }],
                deleted_old_source: None,
            },
        ];

        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        assert!(
            api.modified_closed_in_diff
                .iter()
                .any(|c| c.name == "changed_fn"),
            "grouped use import + 呼び出し更新済みの signature 変更は mod_closed に降格すべき: {api:?}"
        );
        assert!(
            !api.modified.iter().any(|c| c.name == "changed_fn"),
            "changed_fn を blocking な modified に含めるべきでない: {:?}",
            api.modified
        );
    }

    #[test]
    fn is_const_value_only_change_rust_const_value_only_is_true() {
        assert!(is_const_value_only_change(
            "pub const ENEMY_SPEED: f32 = 80.0;",
            "pub const ENEMY_SPEED: f32 = 105.0;",
            "constant",
            crate::language::LangId::Rust,
        ));
    }

    #[test]
    fn is_const_value_only_change_rust_static_value_only_is_true() {
        assert!(is_const_value_only_change(
            "pub static MAX_ALIVE: usize = 200;",
            "pub static MAX_ALIVE: usize = 280;",
            "constant",
            crate::language::LangId::Rust,
        ));
    }

    #[test]
    fn is_const_value_only_change_rust_array_value_only_is_true() {
        assert!(is_const_value_only_change(
            "pub const TABLE: [u8; 3] = [1, 2, 3];",
            "pub const TABLE: [u8; 3] = [4, 5, 6];",
            "constant",
            crate::language::LangId::Rust,
        ));
    }

    #[test]
    fn is_const_value_only_change_rust_static_mut_is_not_demoted() {
        // mutable storage の初期値は状態契約になりやすいため demote しない。
        assert!(!is_const_value_only_change(
            "pub static mut COUNT: usize = 1;",
            "pub static mut COUNT: usize = 2;",
            "constant",
            crate::language::LangId::Rust,
        ));
    }

    #[test]
    fn is_const_value_only_change_rust_type_change_stays_api_mod() {
        // 型変更は shape 変更 → 破壊的の可能性があり api.mod に残す。
        assert!(!is_const_value_only_change(
            "pub const X: f32 = 1.0;",
            "pub const X: f64 = 1.0;",
            "constant",
            crate::language::LangId::Rust,
        ));
    }

    #[test]
    fn is_const_value_only_change_ts_typed_value_only_is_true() {
        assert!(is_const_value_only_change(
            "export const NAME: string = \"a\";",
            "export const NAME: string = \"b\";",
            "variable",
            crate::language::LangId::Typescript,
        ));
    }

    #[test]
    fn is_const_value_only_change_ts_untyped_scalar_is_true() {
        assert!(is_const_value_only_change(
            "export const MAX = 100;",
            "export const MAX = 200;",
            "variable",
            crate::language::LangId::Typescript,
        ));
    }

    #[test]
    fn is_const_value_only_change_ts_untyped_function_stays_api_mod() {
        // 型注釈なし + 関数 initializer は shape 推定が危険なため api.mod に残す。
        assert!(!is_const_value_only_change(
            "export const handler = () => 1;",
            "export const handler = () => 2;",
            "variable",
            crate::language::LangId::Typescript,
        ));
    }

    #[test]
    fn is_const_value_only_change_ts_let_is_not_demoted() {
        assert!(!is_const_value_only_change(
            "export let counter = 1;",
            "export let counter = 2;",
            "variable",
            crate::language::LangId::Typescript,
        ));
    }

    #[test]
    fn is_const_value_only_change_non_binding_kind_is_false() {
        assert!(!is_const_value_only_change(
            "fn foo() -> i32",
            "fn foo() -> u32",
            "function",
            crate::language::LangId::Rust,
        ));
    }

    /// Rust の `pub const` / `pub static` の値 (initializer) のみ変更は破壊的でないため、
    /// blocking な `modified` ではなく informational な `const_value_changes` に振り分けられる
    /// (Issue 2026-06-02-balance-const-value-changes 回帰防止)。
    #[test]
    fn detect_api_changes_rust_const_value_only_is_demoted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[(
                "src/constants.rs",
                "pub const ENEMY_SPEED: f32 = 80.0;\npub static MAX_ALIVE: usize = 200;\n",
            )],
            "initial",
        );
        // 値のみ変更 (shape 不変)
        fs::write(
            repo.join("src/constants.rs"),
            "pub const ENEMY_SPEED: f32 = 105.0;\npub static MAX_ALIVE: usize = 280;\n",
        )
        .expect("write new constants");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/constants.rs".to_string(),
            new_path: "src/constants.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified.is_empty(),
            "値のみ変更の const/static は blocking modified に出すべきでない: {:?}",
            api.modified
        );
        assert!(
            api.const_value_changes
                .iter()
                .any(|c| c.name == "ENEMY_SPEED"),
            "const ENEMY_SPEED の値変更は const_value_changes に出すべき: {:?}",
            api.const_value_changes
        );
        assert!(
            api.const_value_changes
                .iter()
                .any(|c| c.name == "MAX_ALIVE"),
            "static MAX_ALIVE の値変更は const_value_changes に出すべき: {:?}",
            api.const_value_changes
        );
    }

    /// `pub const` の型変更 (shape 変更) は const_value_changes ではなく従来どおり
    /// blocking な modified に残す。
    #[test]
    fn detect_api_changes_rust_const_type_change_stays_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[("src/constants.rs", "pub const LIMIT: u32 = 10;\n")],
            "initial",
        );
        fs::write(
            repo.join("src/constants.rs"),
            "pub const LIMIT: u64 = 10;\n",
        )
        .expect("write new constants");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/constants.rs".to_string(),
            new_path: "src/constants.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified.iter().any(|c| c.name == "LIMIT"),
            "型変更は blocking modified に残すべき: {api:?}"
        );
        assert!(
            api.const_value_changes.is_empty(),
            "型変更は const_value_changes に入れるべきでない: {:?}",
            api.const_value_changes
        );
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        assert!(
            api_changes
                .modified
                .iter()
                .any(|change| change.name == "greet"
                    && change.old_signature.as_deref() == Some("pub fn greet() -> i32")
                    && change.new_signature.as_deref() == Some("pub fn greet(name: &str) -> i32")),
            "rename を含む差分でも関数シグネチャ変更を検出するべき"
        );
    }

    /// 宣言の先頭行が同一でも、複数行に跨る引数列が変わった場合は modified として
    /// 検出される (Issue 2026-05-14-rename-and-multiline-signature の 3a)。
    /// 旧実装は先頭行のみを signature に使っており、引数列が増えても先頭行
    /// (`pub fn foo<F>(`) が同じだと false negative になっていた。
    #[test]
    fn detect_api_changes_modified_includes_multiline_signature_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = "pub fn foo<F>(\n    diff: &str,\n    dir: &str,\n    cb: F,\n) -> Result<(), String>\nwhere\n    F: FnMut() -> Result<(), String>,\n{\n    Ok(())\n}\n";
        fs::write(src_dir.join("foo.rs"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.rs"])
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

        // 引数を 1 つ追加した版 (先頭行 `pub fn foo<F>(` は base と完全一致)
        let after = "pub fn foo<F>(\n    diff: &str,\n    dir: &str,\n    options: &Options,\n    cb: F,\n) -> Result<(), String>\nwhere\n    F: FnMut() -> Result<(), String>,\n{\n    Ok(())\n}\n";
        fs::write(src_dir.join("foo.rs"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.rs".to_string(),
            new_path: "src/foo.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 9,
                new_start: 1,
                new_count: 10,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let foo_change = api_changes
            .modified
            .iter()
            .find(|c| c.name == "foo")
            .expect("foo は multi-line signature の変更で modified に出るべき");
        assert!(
            foo_change
                .old_signature
                .as_deref()
                .map(|s| s.contains("diff: &str") && !s.contains("options"))
                .unwrap_or(false),
            "old_signature は base の引数列のみ含むべき: {:?}",
            foo_change.old_signature
        );
        assert!(
            foo_change
                .new_signature
                .as_deref()
                .map(|s| s.contains("options: &Options"))
                .unwrap_or(false),
            "new_signature は追加された options 引数を含むべき: {:?}",
            foo_change.new_signature
        );
    }

    /// C++ のマクロ呼び出し `BOOST_FOREACH(...) { ... }` は tree-sitter-cpp が関数定義として
    /// 誤パースし、実関数 body 内にネストした偽の function_definition として現れる。引数列が
    /// 変わっても api.mod に出してはならない
    /// (Issue #13: 差分外の BOOST_FOREACH を api_changes.modified に拾う誤検出対策)。
    #[test]
    fn detect_api_changes_cpp_nested_macro_call_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = "void CallInfoManager::Process() {\n    BOOST_FOREACH( const TYPE_CALL_MAP::value_type info, call_inf_map ) {\n        use_it(info.szMyNum);\n    }\n}\n";
        fs::write(src_dir.join("CallInfoManager.cpp"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/CallInfoManager.cpp"])
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

        // BOOST_FOREACH の引数を `call_inf_map` → `this->call_inf_map` に変更しただけ。
        let after = "void CallInfoManager::Process() {\n    BOOST_FOREACH (const TYPE_CALL_MAP::value_type info, this->call_inf_map) {\n        use_it(info.szMyNum);\n    }\n}\n";
        fs::write(src_dir.join("CallInfoManager.cpp"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/CallInfoManager.cpp".to_string(),
            new_path: "src/CallInfoManager.cpp".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            !api_changes
                .modified
                .iter()
                .any(|c| c.name == "BOOST_FOREACH"),
            "BOOST_FOREACH (マクロ誤パース) を api.mod に出すべきではない: {:?}",
            api_changes.modified
        );
    }

    /// C++ のオーバーロード (同名・異シグネチャ) は HashMap<name, sig> で最後の 1 件しか
    /// 残らず、別オーバーロード同士を突き合わせる危険がある。同名が複数あるシンボルは曖昧
    /// として api.mod から除外する (Issue #13)。
    #[test]
    fn detect_api_changes_cpp_overload_excluded_from_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before =
            "int compute(int x) {\n    return x;\n}\nint compute(double x) {\n    return 0;\n}\n";
        fs::write(src_dir.join("calc.cpp"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/calc.cpp"])
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

        // HashMap 代表となる 2 番目のオーバーロードのシグネチャを変更する。
        let after = "int compute(int x) {\n    return x;\n}\nint compute(double x, int y) {\n    return 0;\n}\n";
        fs::write(src_dir.join("calc.cpp"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/calc.cpp".to_string(),
            new_path: "src/calc.cpp".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            !api_changes.modified.iter().any(|c| c.name == "compute"),
            "同名オーバーロード compute は曖昧として modified から除外すべき: {:?}",
            api_changes.modified
        );
    }

    /// 通常の C++ トップレベル関数のシグネチャ変更は #13 の修正後も api.mod に出る。
    /// nested 除外 / 同名複数除外が正常な検出を巻き込まないことの回帰テスト。
    #[test]
    fn detect_api_changes_cpp_real_function_signature_change_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = "int handle(int x) {\n    return x;\n}\n";
        fs::write(src_dir.join("handler.cpp"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/handler.cpp"])
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

        let after = "int handle(int x, int y) {\n    return x + y;\n}\n";
        fs::write(src_dir.join("handler.cpp"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/handler.cpp".to_string(),
            new_path: "src/handler.cpp".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api_changes.modified.iter().any(|c| c.name == "handle"),
            "通常関数 handle の signature 変更は modified に出るべき: {:?}",
            api_changes.modified
        );
    }

    /// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
    /// React 後方互換変更は api.mod に出してはならない (Issue
    /// 引数なし TS/TSX 関数に、`= {}` default 付きの destructured props を追加する
    /// 後方互換変更は api.mod に出してはならない (Issue
    /// 2026-05-28-meet-virtual-you-frontend-modernize 対応)。
    #[test]
    fn detect_api_changes_tsx_no_args_to_destructured_with_default_value_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function TemplateManager() {\n",
            "  return null;\n",
            "}\n"
        );
        fs::write(src_dir.join("TemplateManager.tsx"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/TemplateManager.tsx"])
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

        // 引数なし → destructured props + `= {}` default 付き (省略可能)
        let after = concat!(
            "interface TemplateManagerProps {\n",
            "  onSaved?: (message: string) => void;\n",
            "}\n",
            "export function TemplateManager({ onSaved }: TemplateManagerProps = {}) {\n",
            "  onSaved?.(\"ok\");\n",
            "  return null;\n",
            "}\n"
        );
        fs::write(src_dir.join("TemplateManager.tsx"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/TemplateManager.tsx".to_string(),
            new_path: "src/TemplateManager.tsx".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"TemplateManager"),
            "default `= {{}}` 付きの destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) するが
    /// 型注釈の `interface` が同一ファイル内で全 optional な場合、省略可能と
    /// 判定して api.mod に出してはならない。
    #[test]
    fn detect_api_changes_tsx_no_args_to_destructured_with_all_optional_interface_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function SpeakerNameSetting() {\n",
            "  return null;\n",
            "}\n"
        );
        fs::write(src_dir.join("SpeakerNameSetting.tsx"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/SpeakerNameSetting.tsx"])
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

        // 引数なし → destructured props + 同一ファイル内 interface (全 optional)
        let after = concat!(
            "interface SpeakerNameSettingProps {\n",
            "  onSaved?: (message: string) => void;\n",
            "}\n",
            "export function SpeakerNameSetting({ onSaved }: SpeakerNameSettingProps) {\n",
            "  onSaved?.(\"ok\");\n",
            "  return null;\n",
            "}\n"
        );
        fs::write(src_dir.join("SpeakerNameSetting.tsx"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/SpeakerNameSetting.tsx".to_string(),
            new_path: "src/SpeakerNameSetting.tsx".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"SpeakerNameSetting"),
            "同一ファイル内 interface が全 optional なら destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈の
    /// inline object type に required field を含む場合は破壊的変更として
    /// api.mod に残すべき (副作用回帰防止)。
    #[test]
    fn detect_api_changes_tsx_no_args_to_destructured_with_required_field_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // 引数なし → required field を含む inline object type の destructured props
        let after = concat!(
            "export function widget({ name }: { name: string }) {\n",
            "  return name;\n",
            "}\n",
            "export function caller() { return widget({ name: \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "required field を持つ inline object type の destructured props 追加は api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈が
    /// import 型 (同一ファイル内に declaration なし) の場合は省略可能と断定できない
    /// ため api.mod に残すべき (副作用回帰防止)。
    #[test]
    fn detect_api_changes_tsx_no_args_to_destructured_with_imported_type_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // 引数なし → 同一ファイルに declaration がない type identifier
        let after = concat!(
            "import type { WidgetProps } from './props';\n",
            "export function widget({ name }: WidgetProps) {\n",
            "  return name;\n",
            "}\n",
            "export function caller() { return widget({ name: \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "import 型 (同ファイル内 declaration なし) の destructured props 追加は省略可能と断定できないので api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// TS 関数 destructured params の型注釈 (inline object type) で optional field の
    /// 型を変更した場合 (`{ x?: string }` → `{ x?: number }`) は呼び出し側に見える
    /// 型契約変更なので api.mod に残すべき。「省略可能 destructured を `()` と
    /// 同一視する」過剰正規化を防ぐ codex 指摘 1 への回帰防止。
    #[test]
    fn detect_api_changes_tsx_optional_field_type_change_in_destructured_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function foo({ x }: { x?: string }): string {\n",
            "  return x ?? \"a\";\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), before).expect("write before");
        fs::write(
            src_dir.join("caller.ts"),
            "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
        )
        .expect("write caller");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.ts", "src/caller.ts"])
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

        // optional field の型変更 (string → number)
        let after = concat!(
            "export function foo({ x }: { x?: number }): string {\n",
            "  return String(x ?? 0);\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.ts".to_string(),
            new_path: "src/foo.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"foo"),
            "optional field の型変更は呼び出し側型契約変更なので api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// `interface Props extends Base { ... }` で body のフィールドが全 optional でも、
    /// base interface が required field を持つ可能性があるため省略可能扱いしない
    /// (codex 指摘 2 への回帰防止)。
    #[test]
    fn detect_api_changes_tsx_interface_with_extends_clause_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // interface に extends を付けて props を追加 (body は optional だが base が不明)
        let after = concat!(
            "interface BaseProps {\n",
            "  required: string;\n",
            "}\n",
            "interface WidgetProps extends BaseProps {\n",
            "  optional?: number;\n",
            "}\n",
            "export function widget({ optional }: WidgetProps) {\n",
            "  return optional;\n",
            "}\n",
            "export function caller() { return widget({ required: \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 10,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "extends 持ち interface は base 側の required field を否定できないので api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// 同名 interface declaration merge で、片方が required field を含む場合、
    /// 全体としては省略可能ではないので api.mod に残すべき (codex 指摘 3 への
    /// 回帰防止)。
    #[test]
    fn detect_api_changes_tsx_interface_merge_with_required_field_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // 同名 interface 宣言が 2 つあり、片方は optional のみ、もう片方は required あり
        let after = concat!(
            "interface WidgetProps {\n",
            "  optional?: number;\n",
            "}\n",
            "interface WidgetProps {\n",
            "  required: string;\n",
            "}\n",
            "export function widget({ optional }: WidgetProps) {\n",
            "  return optional;\n",
            "}\n",
            "export function caller() { return widget({ required: \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 10,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "同名 interface merge で required field があれば省略可能ではないので api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// `"name?": string` のような string property name の `?` を optional マーカーと
    /// 誤判定しないこと (codex 指摘 4 への回帰防止)。required field を含む型注釈
    /// なので api.mod に残るべき。
    #[test]
    fn detect_api_changes_tsx_string_property_name_with_question_mark_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // string property name の中に `?` を含む required field を持つ inline object type
        let after = concat!(
            "export function widget(props: { \"name?\": string }) {\n",
            "  return props[\"name?\"];\n",
            "}\n",
            "export function caller() { return widget({ \"name?\": \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "string property name `\"name?\"` の `?` は optional マーカーではなく required field のはず。api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// 旧関数が型注釈内に同名の call signature を含む場合でも、AST で旧 parameters を
    /// 検査して誤判定しないこと (codex 指摘 5 への回帰防止)。旧 sig 文字列に
    /// `foo()` という部分文字列が含まれても、実際の関数 foo は引数を取るので
    /// api.mod に残るべき。
    #[test]
    fn detect_api_changes_tsx_old_signature_contains_inline_call_signature_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        // 旧: 引数あり (引数の型注釈に foo() という inline call signature を含む)
        let before = concat!(
            "export function foo(arg: { foo(): void }) {\n",
            "  arg.foo();\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { foo } from './foo';\nexport const x = foo({ foo: () => {} });\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.ts", "src/user.ts"])
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

        // 新: 引数を destructured + 型注釈に optional のみの inline object に変更
        let after = concat!(
            "export function foo({ x }: { x?: string }) {\n",
            "  return x ?? \"a\";\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.ts".to_string(),
            new_path: "src/foo.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"foo"),
            "旧関数が引数を取る場合は (型注釈内 call signature があっても) api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// ネストしたローカル同名関数を拾わないこと (codex 指摘 6 への回帰防止)。
    /// 変更対象の exported 関数 widget は required props だが、関数内ネストに
    /// 同名 widget があり optional だとしても、トップレベル限定の判定で
    /// api.mod に残すべき。
    #[test]
    fn detect_api_changes_tsx_nested_local_function_does_not_override_top_level_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function widget() {\n",
            "  return null;\n",
            "}\n",
            "export function caller() { return widget(); }\n"
        );
        fs::write(src_dir.join("widget.ts"), before).expect("write before");
        fs::write(
            src_dir.join("user.ts"),
            "import { widget } from './widget';\nexport const x = widget();\n",
        )
        .expect("write user");
        assert!(
            Command::new("git")
                .args(["add", "src/widget.ts", "src/user.ts"])
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

        // 新: トップレベル widget は required props、ネスト widget は optional のみ
        let after = concat!(
            "export function widget({ required }: { required: string }) {\n",
            "  function widget({ optional }: { optional?: string }) {\n",
            "    return optional;\n",
            "  }\n",
            "  return widget({});\n",
            "}\n",
            "export function caller() { return widget({ required: \"x\" }); }\n"
        );
        fs::write(src_dir.join("widget.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/widget.ts".to_string(),
            new_path: "src/widget.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"widget"),
            "トップレベル widget は required props なので、ネスト同名関数に惑わされず api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
    /// React 後方互換変更は api.mod に出してはならない (Issue
    /// 2026-05-28-api-mod-optional-props-additive 対応)。
    #[test]
    fn detect_api_changes_tsx_destructured_props_optional_addition_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string }\n",
            "export function PromptTemplateSelector({ templates, onSelect, className = \"\" }: Props) {\n",
            "  return templates;\n",
            "}\n"
        );
        fs::write(src_dir.join("Selector.tsx"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/Selector.tsx"])
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

        // interface に optional prop を追加し、関数の destructure 受け取りも追加。
        // 型注釈 `: Props` 自体は不変。
        let after = concat!(
            "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string; useExistingContent?: boolean; onChange?: (v: boolean) => void }\n",
            "export function PromptTemplateSelector({ templates, onSelect, className = \"\", useExistingContent = false, onChange }: Props) {\n",
            "  return templates;\n",
            "}\n"
        );
        fs::write(src_dir.join("Selector.tsx"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/Selector.tsx".to_string(),
            new_path: "src/Selector.tsx".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"PromptTemplateSelector"),
            "TSX destructured params の optional 受け取り追加は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// TS 関数の destructured params のデフォルト値変更は signature 不変として扱う
    /// (caller-visible な型契約ではなく binding 時の挙動変更)。
    #[test]
    fn detect_api_changes_typescript_destructured_default_value_change_not_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export interface Opts { x?: number }\n",
            "export function foo({ x = 0 }: Opts) {\n",
            "  return x;\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), before).expect("write before");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.ts"])
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

        let after = concat!(
            "export interface Opts { x?: number }\n",
            "export function foo({ x = 42 }: Opts) {\n",
            "  return x;\n",
            "}\n"
        );
        fs::write(src_dir.join("foo.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.ts".to_string(),
            new_path: "src/foo.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !mod_names.contains(&"foo"),
            "destructured params の default value 変更は api.mod に出してはならない。got: {mod_names:?}"
        );
    }

    /// TS 関数の positional 引数追加は destructure ではなく直接の呼び出し契約変更なので
    /// api.mod に残す (destructure normalize の副作用回帰防止)。
    #[test]
    fn detect_api_changes_typescript_positional_param_added_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function foo(a: number): number {\n",
            "  return a;\n",
            "}\n",
            "export function bar() { return foo(1); }\n"
        );
        fs::write(src_dir.join("foo.ts"), before).expect("write before");
        // 他ファイルからの cross-file 参照を作って closed-in-diff で抑制されないようにする。
        fs::write(
            src_dir.join("caller.ts"),
            "import { foo } from './foo';\nexport const x = foo(1);\n",
        )
        .expect("write caller");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.ts", "src/caller.ts"])
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

        let after = concat!(
            "export function foo(a: number, b: number): number {\n",
            "  return a + b;\n",
            "}\n",
            "export function bar() { return foo(1, 2); }\n"
        );
        fs::write(src_dir.join("foo.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.ts".to_string(),
            new_path: "src/foo.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"foo"),
            "positional 引数追加は destructure ではないので api.mod に残すべき。got: {mod_names:?}"
        );
    }

    /// TS 関数 destructured params の **inline object type** 注釈変更は signature 変更として
    /// 残す (型注釈側は呼び出し側に見える契約)。destructure normalize が type_annotation
    /// に踏み込まないことの回帰防止。
    #[test]
    fn detect_api_changes_typescript_inline_object_type_change_is_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let before = concat!(
            "export function foo({ x }: { x: string }): string {\n",
            "  return x;\n",
            "}\n",
            "export function bar() { return foo({ x: 'a' }); }\n"
        );
        fs::write(src_dir.join("foo.ts"), before).expect("write before");
        fs::write(
            src_dir.join("caller.ts"),
            "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
        )
        .expect("write caller");
        assert!(
            Command::new("git")
                .args(["add", "src/foo.ts", "src/caller.ts"])
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

        // inline object type に required な y フィールドを追加 (breaking)
        let after = concat!(
            "export function foo({ x, y }: { x: string; y: number }): string {\n",
            "  return x + y;\n",
            "}\n",
            "export function bar() { return foo({ x: 'a', y: 1 }); }\n"
        );
        fs::write(src_dir.join("foo.ts"), after).expect("write after");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.ts".to_string(),
            new_path: "src/foo.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let mod_names: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            mod_names.contains(&"foo"),
            "inline object type 注釈の構造変更は api.mod に残すべき。got: {mod_names:?}"
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

    // --- git worktree 判定 & 非 git ディレクトリの graceful skip ---

    #[test]
    fn is_git_work_tree_true_inside_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo_for_test(dir.path());
        assert!(
            is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
            "git init 済み dir は worktree 内"
        );
    }

    #[test]
    fn is_git_work_tree_false_outside_repo() {
        // git init しない一時 dir は管理外。
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            !is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
            "git 管理外 dir は Ok(false)"
        );
    }

    #[test]
    fn resolve_git_diff_skips_non_git_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve_git_diff(dir.path().to_str().expect("utf-8"), "HEAD", false).expect("resolve")
        {
            GitDiffInput::Skipped(skip) => {
                assert_eq!(skip.reason.as_str(), "not_git_repository");
                assert_eq!(skip.source.as_str(), "git");
            }
            GitDiffInput::Diff(_) => panic!("非 git dir では Skipped を返すべき"),
        }
    }

    #[test]
    fn resolve_git_diff_rejects_invalid_base_even_when_non_git() {
        // base 不正は git 管理外でも入力契約違反として弾く (skip より優先)。
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            resolve_git_diff(dir.path().to_str().expect("utf-8"), "-x", false).is_err(),
            "先頭 '-' の base は非 git でも Err"
        );
    }

    #[test]
    fn resolve_blame_source_files_skips_non_git_without_explicit_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve_blame_source_files(
            dir.path().to_str().expect("utf-8"),
            true,
            None,
            None,
            None,
            &[],
        )
        .expect("resolve")
        {
            BlameSourceResolution::Skipped(skip) => {
                assert_eq!(skip.reason.as_str(), "not_git_repository");
            }
            BlameSourceResolution::Files(f) => panic!("非 git + 明示 paths 無しは Skipped: {f:?}"),
        }
    }

    #[test]
    fn resolve_blame_source_files_keeps_explicit_paths_when_non_git() {
        // 管理外でも --paths 明示があれば skip せず明示分を返す (明示優先)。
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve_blame_source_files(
            dir.path().to_str().expect("utf-8"),
            true,
            None,
            Some("a.rs,b.rs"),
            None,
            &[],
        )
        .expect("resolve")
        {
            BlameSourceResolution::Files(f) => {
                assert!(f.contains(&"a.rs".to_string()));
                assert!(f.contains(&"b.rs".to_string()));
            }
            BlameSourceResolution::Skipped(_) => panic!("明示 paths があれば skip しない"),
        }
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
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

    /// Tauri command の自動注入型引数 (AppHandle) 追加は JS-facing signature 不変なので
    /// api.mod / mod_closed のどちらにも出ない (パターンB)。
    #[test]
    fn detect_api_changes_tauri_command_injected_arg_addition_not_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "src-tauri/Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
                (
                    "src-tauri/src/cmd.rs",
                    "#[tauri::command]\npub fn get_status(id: u32) -> String {\n    String::new()\n}\n",
                ),
            ],
            "base",
        );
        fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn get_status(app: tauri::AppHandle, id: u32) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src-tauri/src/cmd.rs".to_string(),
            new_path: "src-tauri/src/cmd.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("get_status"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("get_status"));
        assert!(
            !flagged,
            "Tauri 自動注入引数の追加は signature 差分にしない。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
        );
    }

    /// Tauri command でも通常引数の追加は呼び出し契約を変えるため signature 差分として検出される。
    #[test]
    fn detect_api_changes_tauri_command_regular_arg_addition_is_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "src-tauri/Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
                (
                    "src-tauri/src/cmd.rs",
                    "#[tauri::command]\npub fn get_status(id: u32) -> String {\n    String::new()\n}\n",
                ),
            ],
            "base",
        );
        fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn get_status(id: u32, verbose: bool) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src-tauri/src/cmd.rs".to_string(),
            new_path: "src-tauri/src/cmd.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("get_status"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("get_status"));
        assert!(
            flagged,
            "通常引数の追加は signature 差分として検出されるべき"
        );
    }

    /// Channel<T> は JS 側から渡す引数なので Tauri 自動注入から除外せず signature 差分に残す。
    #[test]
    fn detect_api_changes_tauri_command_channel_arg_is_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "src-tauri/Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
                (
                    "src-tauri/src/cmd.rs",
                    "#[tauri::command]\npub fn watch(id: u32) -> String {\n    String::new()\n}\n",
                ),
            ],
            "base",
        );
        fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn watch(id: u32, on_event: Channel<String>) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src-tauri/src/cmd.rs".to_string(),
            new_path: "src-tauri/src/cmd.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("watch"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("watch"));
        assert!(
            flagged,
            "Channel<T> 引数は除外せず signature 差分に残すべき"
        );
    }

    /// 全 cross-file 参照が同一 diff 内の変更 hunk で追随済みの api.mod は
    /// modified_closed_in_diff (informational) に降格する (パターンA)。
    #[test]
    fn detect_api_changes_modified_with_all_callers_in_diff_is_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod detector;\npub mod manager;\n"),
                (
                    "src/detector.rs",
                    "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
                ),
                (
                    "src/manager.rs",
                    "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1)\n}\n",
                ),
            ],
            "base",
        );
        // create_detector に引数追加 + caller (manager.rs) を同一 diff で追随更新
        fs::write(
            repo.join("src/detector.rs"),
            "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
        )
        .expect("write");
        fs::write(
            repo.join("src/manager.rs"),
            "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1, true)\n}\n",
        )
        .expect("write");
        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/detector.rs".to_string(),
                new_path: "src/detector.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 3,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "src/manager.rs".to_string(),
                new_path: "src/manager.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 4,
                    new_start: 1,
                    new_count: 4,
                }],
                deleted_old_source: None,
            },
        ];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("create_detector")),
            "全 caller が同一 diff 内なら modified_closed_in_diff に降格すべき。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
        );
        assert!(
            !api.modified
                .iter()
                .any(|m| m.name.ends_with("create_detector")),
            "closed-in-diff は blocking な modified に残さない"
        );
    }

    /// caller が diff 外 (変更 hunk に含まれない) に残る api.mod は blocking な modified のまま。
    #[test]
    fn detect_api_changes_modified_with_caller_outside_diff_stays_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod detector;\npub mod manager;\n"),
                (
                    "src/detector.rs",
                    "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
                ),
                (
                    "src/manager.rs",
                    "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1)\n}\n",
                ),
            ],
            "base",
        );
        // detector.rs のみシグネチャ変更。manager.rs (caller) は未更新かつ diff にも含めない。
        fs::write(
            repo.join("src/detector.rs"),
            "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/detector.rs".to_string(),
            new_path: "src/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified
                .iter()
                .any(|m| m.name.ends_with("create_detector")),
            "diff 外に未更新 caller が残る場合は blocking な modified に残すべき。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
        );
    }

    /// rename された caller で呼び出しが古いまま残る場合は blocking。closed-in-diff の変更行
    /// 判定が rename-aware (git diff -M) で、rename を新規全行追加と誤認しないことを検証する
    /// (codex 指摘: new_path 単独 pathspec だと未更新呼び出しまで changed に見える)。
    #[test]
    fn detect_api_changes_renamed_caller_with_unchanged_call_stays_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod api;\npub mod caller;\n"),
                (
                    "src/api.rs",
                    "pub fn process(id: u32) -> u32 {\n    id\n}\n",
                ),
                (
                    "src/caller.rs",
                    "use crate::api::process;\npub fn run() -> u32 {\n    process(1)\n}\n",
                ),
            ],
            "base",
        );
        // process に引数追加 (signature 変更)
        fs::write(
            repo.join("src/api.rs"),
            "pub fn process(id: u32, extra: bool) -> u32 {\n    id\n}\n",
        )
        .expect("write");
        // caller.rs を caller2.rs に rename + 無関係コメント追加。process(1) 呼び出しは古いまま。
        std::fs::remove_file(repo.join("src/caller.rs")).expect("rm");
        fs::write(
            repo.join("src/caller2.rs"),
            "use crate::api::process;\n// unrelated comment line\npub fn run() -> u32 {\n    process(1)\n}\n",
        )
        .expect("write");
        fs::write(repo.join("src/lib.rs"), "pub mod api;\npub mod caller2;\n").expect("write");
        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/api.rs".to_string(),
                new_path: "src/api.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 3,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "src/caller.rs".to_string(),
                new_path: "src/caller2.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 4,
                    new_start: 1,
                    new_count: 5,
                }],
                deleted_old_source: None,
            },
        ];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified.iter().any(|m| m.name.ends_with("process")),
            "rename + 未更新呼び出しが残る場合は blocking。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
        );
    }

    /// Swift の internal 型 (public/open でない) は外部 API ではないため api.add に出さない。
    /// public 型は引き続き出す (パターンD: sidecar/executable 内部型を api.add に出さない)。
    #[test]
    fn detect_api_changes_swift_internal_type_excluded_from_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(repo, &[("README.md", "init\n")], "base");
        fs::write(
            repo.join("helper.swift"),
            "enum DetectionError: Error {\n    case failed\n}\npublic struct Detector {\n    public func run() -> Int { 0 }\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "helper.swift".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.iter().any(|n| n.ends_with("DetectionError")),
            "Swift internal enum は api.add に出ない。got: {added:?}"
        );
        assert!(
            added.iter().any(|n| n.contains("Detector")),
            "Swift public struct は api.add に出る。got: {added:?}"
        );
    }

    /// Swift の public protocol requirement の signature 変更は外部公開 API 変更なので
    /// api 差分 (mod / mod_closed) に出る (codex 指摘2 の false negative 回避)。
    #[test]
    fn detect_api_changes_swift_public_protocol_requirement_signature_change_is_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[(
                "Service.swift",
                "public protocol Service {\n    func handle() -> Int\n}\n",
            )],
            "base",
        );
        fs::write(
            repo.join("Service.swift"),
            "public protocol Service {\n    func handle(_ value: Int) -> Int\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "Service.swift".to_string(),
            new_path: "Service.swift".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("handle"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("handle"));
        assert!(
            flagged,
            "public protocol requirement の signature 変更は api.mod に出る。mod={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    /// 複数行の Swift protocol requirement でも signature 変更が AST 抽出で検出される
    /// (先頭行 fallback では 2 行目以降の型変更を見逃す、codex 指摘)。
    #[test]
    fn detect_api_changes_swift_multiline_protocol_requirement_signature_change_is_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[(
                "Service.swift",
                "public protocol Service {\n    func handle(\n        _ value: Int\n    ) -> Int\n}\n",
            )],
            "base",
        );
        // 2 行目の型のみ Int → String に変更 (先頭行 `func handle(` は不変)
        fs::write(
            repo.join("Service.swift"),
            "public protocol Service {\n    func handle(\n        _ value: String\n    ) -> Int\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "Service.swift".to_string(),
            new_path: "Service.swift".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("handle"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("handle"));
        assert!(
            flagged,
            "複数行 protocol requirement の型変更も api.mod に出る。mod={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    /// private module (`mod meeting;`) 配下の新規 pub fn は crate 外から到達できないため
    /// api.add に出さない (パターンC)。
    #[test]
    fn detect_api_changes_private_module_pub_fn_excluded_from_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "mod meeting;\n"),
                ("src/meeting/mod.rs", "pub mod detector;\n"),
            ],
            "base",
        );
        fs::write(
            repo.join("src/meeting/detector.rs"),
            "pub fn create_detector() -> u32 {\n    0\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/meeting/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !added.iter().any(|n| n.ends_with("create_detector")),
            "private module 配下の pub fn は api.add に出ない。got: {added:?}"
        );
    }

    /// `pub mod` 経路で到達可能なモジュール配下の新規 pub fn は api.add に出る。
    #[test]
    fn detect_api_changes_public_module_pub_fn_included_in_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod meeting;\n"),
                ("src/meeting/mod.rs", "pub mod detector;\n"),
            ],
            "base",
        );
        fs::write(
            repo.join("src/meeting/detector.rs"),
            "pub fn create_detector() -> u32 {\n    0\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/meeting/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            added.iter().any(|n| n.ends_with("create_detector")),
            "pub mod 経路で到達可能な pub fn は api.add に出る。got: {added:?}"
        );
    }

    /// private module でも root から `pub use` re-export されていれば外部到達可能なので api.add に出る。
    #[test]
    fn detect_api_changes_private_module_with_pub_use_reexport_included() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                (
                    "src/lib.rs",
                    "mod meeting;\npub use meeting::detector::create_detector;\n",
                ),
                ("src/meeting/mod.rs", "pub mod detector;\n"),
            ],
            "base",
        );
        fs::write(
            repo.join("src/meeting/detector.rs"),
            "pub fn create_detector() -> u32 {\n    0\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/meeting/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            added.iter().any(|n| n.ends_with("create_detector")),
            "pub use re-export された pub fn は api.add に出る。got: {added:?}"
        );
    }

    /// new と base 両方で private module 配下の pub fn の signature 変更は api.mod に出さない。
    #[test]
    fn detect_api_changes_private_module_signature_change_excluded_from_mod() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "mod meeting;\n"),
                ("src/meeting/mod.rs", "pub mod detector;\n"),
                (
                    "src/meeting/detector.rs",
                    "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
                ),
            ],
            "base",
        );
        fs::write(
            repo.join("src/meeting/detector.rs"),
            "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
        )
        .expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/meeting/detector.rs".to_string(),
            new_path: "src/meeting/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api
            .modified
            .iter()
            .any(|m| m.name.ends_with("create_detector"))
            || api
                .modified_closed_in_diff
                .iter()
                .any(|m| m.name.ends_with("create_detector"));
        assert!(
            !flagged,
            "new/base 両方 private module 配下の signature 変更は api.mod に出ない。mod={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    /// base で公開 (pub mod) だったモジュールを同 diff で private 化しつつ配下 pub fn の
    /// signature を変えた場合、旧 API の破壊的変更なので api.mod に残す (codex 指摘2)。
    #[test]
    fn detect_api_changes_module_made_private_in_diff_keeps_mod_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod meeting;\n"),
                ("src/meeting/mod.rs", "pub mod detector;\n"),
                (
                    "src/meeting/detector.rs",
                    "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
                ),
            ],
            "base",
        );
        // meeting を private 化 (pub mod → mod) しつつ create_detector の signature を変更
        fs::write(repo.join("src/lib.rs"), "mod meeting;\n").expect("write");
        fs::write(
            repo.join("src/meeting/detector.rs"),
            "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
        )
        .expect("write");
        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/lib.rs".to_string(),
                new_path: "src/lib.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "src/meeting/detector.rs".to_string(),
                new_path: "src/meeting/detector.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 3,
                }],
                deleted_old_source: None,
            },
        ];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified
                .iter()
                .any(|m| m.name.ends_with("create_detector")),
            "base で公開だったモジュールの private 化 + signature 変更は blocking。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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

        // 相殺された 3 関数は moved として informational に提示されるべき
        let moved_names: std::collections::HashSet<&str> =
            api_changes.moved.iter().map(|m| m.name.as_str()).collect();
        for name in ["iter_plugin_manifests", "check_layout", "main"] {
            assert!(
                moved_names.contains(name),
                "相殺された関数は moved に積まれるべき。got moved: {moved_names:?}"
            );
        }
        for m in &api_changes.moved {
            assert_eq!(m.from, "scripts/regenerate_marketplace.py");
            assert_eq!(m.to, "scripts/marketplace.py");
        }
    }

    #[test]
    fn detect_api_changes_uses_diff_old_source_when_git_show_fails() {
        // CI 環境で source branch (削除コミット適用後) が HEAD の状態で `--base HEAD` を
        // 渡したケースを再現する。`git show HEAD:old_path` は失敗するが、
        // `--diff-file` 経由で渡された削除 hunk から旧ソースを復元できれば
        // api_changes.removed に反映されるべき。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // 旧ファイルを base にコミット → さらに削除を HEAD としてコミット。
        // `git show HEAD:src/old.py` は HEAD には存在しないため失敗する。
        git_commit_files(
            repo,
            &[("src/old.py", "def removed_fn():\n    return 1\n")],
            "initial",
        );
        fs::remove_file(repo.join("src/old.py")).expect("rm");
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo)
            .status()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "delete"])
            .current_dir(repo)
            .status()
            .expect("git commit");

        // hunk から復元される旧ソース (`-` 行から組み立て)
        let deleted_src = b"def removed_fn():\n    return 1\n".to_vec();
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/old.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(deleted_src),
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"removed_fn"),
            "diff の deleted_old_source からシンボルが復元されるべき。got: {removed:?}"
        );
    }

    #[test]
    fn detect_api_changes_skips_removed_when_no_old_source_available() {
        // `git show base:old_path` が失敗し、かつ deleted_old_source も無い場合は
        // 従来通り何も報告しない (false positive を出さない)。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(repo, &[("README.md", "# repo\n")], "initial");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/old.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api_changes.removed.is_empty(),
            "旧ソース取得不能時は removed に出すべきではない"
        );
    }

    #[test]
    fn detect_api_changes_module_to_package_split_reports_moved_not_removed() {
        // 報告再現: cli.py を cli/ パッケージに分割し、各サブコマンドを
        // cli/_commands/<name>.py に移動。cli/__init__.py は再エクスポートを行う。
        // 旧 cli.py の関数は削除ではなく moved として報告されるべき。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let old_cli = "\
import typer

app = typer.Typer()

@app.command(\"rotate\")
def rotate_command(name: str):
    pass

@app.command(\"list\")
def list_tokens():
    pass

@app.command(\"check\")
def check_command():
    pass

def main():
    app()
";
        git_commit_files(repo, &[("src/token_manager/cli.py", old_cli)], "initial");

        // 旧 cli.py を削除し、cli/ パッケージに分割
        fs::remove_file(repo.join("src/token_manager/cli.py")).expect("rm old");
        fs::create_dir_all(repo.join("src/token_manager/cli/_commands")).expect("create pkg");

        let init_py = "\
import typer

from ._commands.rotate import rotate_command
from ._commands.list import list_tokens
from ._commands.check import check_command

app = typer.Typer()

app.command(\"rotate\")(rotate_command)
app.command(\"list\")(list_tokens)
app.command(\"check\")(check_command)


def main():
    app()
";
        let rotate_py = "\
def rotate_command(name: str):
    pass
";
        let list_py = "\
def list_tokens():
    pass
";
        let check_py = "\
def check_command():
    pass
";
        fs::write(repo.join("src/token_manager/cli/__init__.py"), init_py).expect("write init");
        fs::write(repo.join("src/token_manager/cli/_commands/__init__.py"), "")
            .expect("write _commands init");
        fs::write(
            repo.join("src/token_manager/cli/_commands/rotate.py"),
            rotate_py,
        )
        .expect("write rotate");
        fs::write(
            repo.join("src/token_manager/cli/_commands/list.py"),
            list_py,
        )
        .expect("write list");
        fs::write(
            repo.join("src/token_manager/cli/_commands/check.py"),
            check_py,
        )
        .expect("write check");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/token_manager/cli.py".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 20,
                    new_start: 0,
                    new_count: 0,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/token_manager/cli/__init__.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 13,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/token_manager/cli/_commands/__init__.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 0,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/token_manager/cli/_commands/rotate.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 2,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/token_manager/cli/_commands/list.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 2,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/token_manager/cli/_commands/check.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 2,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: std::collections::HashSet<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        // 移動した関数は api.rm から消えていること（report 再現のコア）
        for name in ["rotate_command", "list_tokens", "check_command", "main"] {
            assert!(
                !removed_names.contains(name),
                "module → package 化で移動したシンボルは api.rm に残らないべき。got removed: {removed_names:?}"
            );
        }

        // 移動した関数は moved に積まれていること
        let moved_by_name: std::collections::HashMap<&str, &crate::models::review::MovedSymbol> =
            api_changes
                .moved
                .iter()
                .map(|m| (m.name.as_str(), m))
                .collect();
        for name in ["rotate_command", "list_tokens", "check_command", "main"] {
            let m = moved_by_name
                .get(name)
                .unwrap_or_else(|| panic!("{name} が moved に含まれていない: {moved_by_name:?}"));
            assert_eq!(
                m.from, "src/token_manager/cli.py",
                "from は旧 cli.py であるべき"
            );
            assert!(
                m.to.starts_with("src/token_manager/cli/"),
                "to は新パッケージ配下であるべき: {}",
                m.to
            );
        }
    }

    #[test]
    fn detect_api_changes_python_property_to_field_replacement_is_not_removed() {
        // 報告再現: Python の `@property def x(self) -> str` を `@dataclass` フィールド
        // `x: str` に置き換えると、`obj.x` 属性アクセス API は維持されるため
        // `api.rm` ではなく `property_to_field` カテゴリに分類されるべき。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let old_content = "\
from dataclasses import dataclass
from urllib.parse import urlparse


@dataclass
class ReviewConfig:
    project_url: str

    @property
    def gitlab_base_url(self) -> str:
        parsed = urlparse(self.project_url)
        return f\"{parsed.scheme}://{parsed.netloc}\"
";
        git_commit_files(repo, &[("scripts/review_mr.py", old_content)], "initial");

        let new_content = "\
from dataclasses import dataclass


@dataclass
class ReviewConfig:
    project_url: str
    gitlab_base_url: str
";
        fs::write(repo.join("scripts/review_mr.py"), new_content).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "scripts/review_mr.py".to_string(),
            new_path: "scripts/review_mr.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 12,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: std::collections::HashSet<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !removed_names.contains(&"ReviewConfig.gitlab_base_url"),
            "@property → dataclass field 置き換えは api.rm に残らないべき。got: {removed_names:?}"
        );

        let p2f_names: Vec<&str> = api_changes
            .property_to_field
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(
            p2f_names.contains(&"ReviewConfig.gitlab_base_url"),
            "@property → dataclass field 置き換えは property_to_field に積まれるべき。got: {p2f_names:?}"
        );
    }

    #[test]
    fn detect_api_changes_python_property_removed_without_field_remains_removed() {
        // 安全網: クラスから @property を削除し、対応するフィールドも追加しない場合は
        // 通常通り api.rm として残るべき。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let old_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str

    @property
    def computed(self) -> str:
        return self.name.upper()
";
        git_commit_files(repo, &[("foo.py", old_content)], "initial");

        let new_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str
";
        fs::write(repo.join("foo.py"), new_content).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "foo.py".to_string(),
            new_path: "foo.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 10,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: std::collections::HashSet<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed_names.contains(&"Foo.computed"),
            "対応 field が無い @property 削除は api.rm に残るべき。got: {removed_names:?}"
        );
        assert!(
            api_changes.property_to_field.is_empty(),
            "対応 field が無い場合は property_to_field に積まれないべき。got: {:?}",
            api_changes.property_to_field
        );
    }

    #[test]
    fn extract_python_class_fields_collects_typed_annotations_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let py = "\
from dataclasses import dataclass


@dataclass
class A:
    x: int
    y: str = \"default\"
    untyped = 1


class B:
    z: float
";
        fs::write(dir.path().join("m.py"), py).expect("write");

        let a_fields =
            extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "A");
        assert!(
            a_fields.contains("x"),
            "typed annotation は採取される: {a_fields:?}"
        );
        assert!(
            a_fields.contains("y"),
            "default 値付き typed annotation も採取される: {a_fields:?}"
        );
        assert!(
            !a_fields.contains("untyped"),
            "type annotation が無い代入は採取しない: {a_fields:?}"
        );

        let b_fields =
            extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "B");
        assert!(
            b_fields.contains("z"),
            "@dataclass でないクラスでも採取する: {b_fields:?}"
        );

        let none =
            extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "Missing");
        assert!(none.is_empty(), "存在しないクラス名は空集合: {none:?}");
    }

    /// 他ファイルから参照されていない exported シンボルを削除した場合、
    /// `removed` ではなく `removed_dead` カテゴリに振り分けられること
    /// (Issue 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
    /// HEAD ツリーで参照 0 件 = repo 内 dead removal を informational として提示。
    #[test]
    fn detect_api_changes_unreferenced_removal_goes_to_removed_dead_not_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // foo / bar 両方を定義。caller なし (dead-code 想定)。
        git_commit_files(
            repo,
            &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
            "initial",
        );
        // bar を削除 (HEAD で bar への参照は 0 件)
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_dead_names: Vec<&str> = api_changes
            .removed_dead
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let removed_names: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed_dead_names.contains(&"bar"),
            "HEAD で参照 0 件の削除は removed_dead に振り分けられるべき。got removed_dead: {removed_dead_names:?}, removed: {removed_names:?}"
        );
        assert!(
            !removed_names.contains(&"bar"),
            "removed_dead に振り分けられた symbol は removed には残ってはならない。got: {removed_names:?}"
        );
    }

    /// HEAD ツリーで他ファイルから参照されているシンボル (alive) の削除は、
    /// `removed_dead` ではなく `removed` に残ること (副作用回帰防止)。
    /// 「破壊的削除」と「dead-code 整理」の区別が機能していることを確認。
    #[test]
    fn detect_api_changes_referenced_removal_stays_in_removed_not_dead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // foo / bar を定義。caller.py で bar を参照 (alive)。
        git_commit_files(
            repo,
            &[
                ("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n"),
                ("caller.py", "from mod import bar\nbar()\n"),
            ],
            "initial",
        );
        // bar を削除 (caller.py はそのままで bar への参照を維持 = 破壊的削除)
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let removed_dead_names: Vec<&str> = api_changes
            .removed_dead
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed_names.contains(&"bar"),
            "HEAD で参照ありのシンボル削除は removed (破壊的削除) に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
        );
        assert!(
            !removed_dead_names.contains(&"bar"),
            "参照ありの削除は removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
        );
    }

    /// detect_api_changes の早期 continue 経路 (closed-in-diff for api.rm) でも
    /// qualname 対応が機能すること (codex 2 回目指摘への回帰防止)。
    /// 「qualname method 削除 + 同ファイルに新規関数追加 + 外部 caller 残存」のケースで
    /// removed_dead に誤分類されず removed に残る。
    #[test]
    fn detect_api_changes_qualname_method_with_inline_addition_and_external_caller_stays_in_removed()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // 旧: Foo.bar あり、caller.py で Foo().bar() を参照
        git_commit_files(
            repo,
            &[
                (
                    "foo.py",
                    "class Foo:\n    def bar(self):\n        return 1\n",
                ),
                (
                    "caller.py",
                    "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
                ),
            ],
            "initial",
        );
        // 新: bar を削除し、同ファイルに新規関数 helper を追加
        // → new_symbols_in_current_file が空でないので closed-in-diff 早期 continue
        //   経路に入る (line 1836 周辺)
        fs::write(
            repo.join("foo.py"),
            "class Foo:\n    pass\n\n\ndef helper():\n    return 0\n",
        )
        .expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "foo.py".to_string(),
            new_path: "foo.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let removed_dead_names: Vec<&str> = api_changes
            .removed_dead
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        // 早期 continue 経路でも bare name + def_count 判定が効く
        assert!(
            removed_names.iter().any(|n| n.contains("bar")),
            "qualname method 削除 + 同ファイル新規追加 + 外部 caller 残存は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
        );
        assert!(
            !removed_dead_names.iter().any(|n| n.contains("bar")),
            "上記ケースを removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
        );
    }

    /// qualname (`Container.method`) 形式の class method 削除でも、別ファイルから
    /// bare name で参照されていれば破壊的削除として `removed` に残ること
    /// (codex 指摘 1: qualname 誤分類への回帰防止)。
    #[test]
    fn detect_api_changes_qualname_method_with_external_caller_stays_in_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // class Foo の method bar を削除するが、caller.py で Foo().bar() を呼んでいる
        git_commit_files(
            repo,
            &[
                (
                    "foo.py",
                    "class Foo:\n    def bar(self):\n        return 1\n",
                ),
                (
                    "caller.py",
                    "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
                ),
            ],
            "initial",
        );
        // method bar を削除
        fs::write(repo.join("foo.py"), "class Foo:\n    pass\n").expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "foo.py".to_string(),
            new_path: "foo.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed_names: Vec<&str> = api_changes
            .removed
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let removed_dead_names: Vec<&str> = api_changes
            .removed_dead
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        // bare name 'bar' で検索すると caller.py の Foo().bar() で参照あり
        // qualname を bare で正規化していなければ常に refs 0 件で removed_dead に
        // 誤分類される
        assert!(
            removed_names.iter().any(|n| n.contains("bar")),
            "外部 caller がいる qualname method 削除は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
        );
        assert!(
            !removed_dead_names.iter().any(|n| n.contains("bar")),
            "外部 caller がいる qualname method 削除を removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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

    /// Angular component の public method が `templateUrl` で紐づく
    /// `.component.html` から参照されている場合、dead 判定から除外される。
    ///
    /// 再現元: astro-sight-bug-reports#4 (framework-template-ref)
    /// - `@Component({ templateUrl: './foo.component.html' })` で紐づく HTML 内の
    ///   `(event)="method()"` / `[prop]="method()"` / `[ngStyle]="{ ...: method() }"`
    ///   等の binding 式で呼ばれている component method が
    ///   TS AST だけ見ると dead 扱いされる問題を修正。
    #[test]
    fn detect_dead_excludes_angular_component_methods_referenced_from_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // Angular プロジェクト標識として angular.json を置く
        fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

        let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-sample',
    templateUrl: './sample.component.html',
})
export class SampleComponent {
    public headerCheck: boolean = false;

    public headerCheckChanged(): void {
    }

    public isHeaderDisabled(): boolean {
        return false;
    }

    public reallyUnusedMethod(): void {
    }
}
"#;
        let component_html = r#"
<label [ngStyle]="{'display': isHeaderDisabled() ? 'none' : ''}">
    <input type="checkbox"
           [(ngModel)]="headerCheck"
           (ngModelChange)="headerCheckChanged()">
</label>
"#;
        fs::write(repo.join("sample.component.ts"), component_ts).expect("write ts");
        fs::write(repo.join("sample.component.html"), component_html).expect("write html");

        let files = vec![repo.join("sample.component.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.iter().any(|n| n.ends_with("headerCheckChanged")),
            "Angular template から (ngModelChange) で参照される method は dead から除外されるべき。got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("isHeaderDisabled")),
            "Angular template の [ngStyle] 式から参照される method は dead から除外されるべき。got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("reallyUnusedMethod")),
            "テンプレートからも参照されない method は dead として検出されるべき。got: {names:?}"
        );
    }

    /// C/C++ の前方宣言・opaque tag (`typedef struct st_mysql MYSQL;` の `st_mysql`) は
    /// 「定義」ではなく宣言なので dead_symbols に含めない。本体を持つ未使用 struct は
    /// 引き続き dead として検出される (Issue #11)。
    #[test]
    fn detect_dead_cpp_forward_declaration_tag_excluded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let header = "typedef struct st_mysql MYSQL;\nstruct UnusedDefined { int x; };\n";
        fs::write(repo.join("mysql_service.h"), header).expect("write header");

        let files = vec![repo.join("mysql_service.h")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"st_mysql"),
            "前方宣言タグ st_mysql は dead に含めない: {names:?}"
        );
        assert!(
            names.contains(&"UnusedDefined"),
            "本体を持つ未使用 struct UnusedDefined は dead として検出されるべき: {names:?}"
        );
    }

    /// C/C++ の enum は、型名が直接使われなくても列挙子のいずれかが参照されていれば live と
    /// 判定する。body あり typedef tag も alias 名経由の参照で live と判定する。列挙子も alias も
    /// 未使用なら dead として検出される (Issue #12 enumerator liveness / Issue #11 typedef alias)。
    #[test]
    fn detect_dead_cpp_enum_enumerator_and_typedef_alias_liveness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let header = "enum StdAgentSatus { POST_WORK = 1, LOGOFF = 10 };\n\
enum UnusedEnum { UE_A = 1, UE_B = 2 };\n\
typedef struct st_local { int v; } LocalAlias;\n\
typedef struct st_unused { int w; } UnusedAlias;\n";
        let main_cpp = "#include \"svc.h\"\n\
int useThem() {\n\
    int x = LOGOFF;\n\
    LocalAlias la;\n\
    la.v = 1;\n\
    return x + la.v;\n\
}\n";
        git_commit_files(
            repo,
            &[("svc.h", header), ("main.cpp", main_cpp)],
            "initial",
        );

        let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"StdAgentSatus"),
            "列挙子 LOGOFF が使用中の enum StdAgentSatus は dead に出さない: {names:?}"
        );
        assert!(
            !names.contains(&"st_local"),
            "alias LocalAlias が使用中の typedef tag st_local は dead に出さない: {names:?}"
        );
        assert!(
            names.contains(&"UnusedEnum"),
            "列挙子も未使用の enum UnusedEnum は dead として検出されるべき: {names:?}"
        );
        assert!(
            names.contains(&"st_unused"),
            "alias 未使用の typedef tag st_unused は dead として検出されるべき: {names:?}"
        );
    }

    /// codex 指摘の回帰: (1) typedef の配列長式で参照される列挙子は def 誤判定されず enum が
    /// live、(2) 複数 declarator (`typedef S A, *B;`) のいずれかの alias 使用で underlying tag が
    /// live と判定される (Issue #11/#12)。
    #[test]
    fn detect_dead_cpp_typedef_array_size_and_multiple_declarators() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let header = "enum Sz { SZ_VAL = 4 };\n\
typedef int IntArr[SZ_VAL];\n\
typedef struct st_multi { int v; } MultiA, *MultiBPtr;\n\
typedef struct st_solo { int w; } SoloAlias;\n";
        let main_cpp = "#include \"svc.h\"\n\
IntArr g_arr;\n\
int useMulti() {\n\
    MultiBPtr p = nullptr;\n\
    return p ? 1 : 0;\n\
}\n";
        git_commit_files(
            repo,
            &[("svc.h", header), ("main.cpp", main_cpp)],
            "initial",
        );

        let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"Sz"),
            "typedef 配列長 IntArr[SZ_VAL] で参照される列挙子の enum Sz は live: {names:?}"
        );
        assert!(
            !names.contains(&"st_multi"),
            "複数 declarator の 2 番目 alias MultiBPtr 使用で st_multi は live: {names:?}"
        );
        assert!(
            names.contains(&"st_solo"),
            "alias SoloAlias 未使用の st_solo は dead として検出されるべき: {names:?}"
        );
    }

    /// Angular の inline template (`@Component({ template: \`...\` })`) で参照される
    /// component method も dead 判定から除外される。
    #[test]
    fn detect_dead_excludes_angular_inline_template_method_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

        let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-inline',
    template: `<button (click)="onClick()">{{ greeting }}</button>`,
})
export class InlineComponent {
    public greeting: string = 'hi';

    public onClick(): void {
    }

    public reallyUnusedInline(): void {
    }
}
"#;
        fs::write(repo.join("inline.component.ts"), component_ts).expect("write ts");

        let files = vec![repo.join("inline.component.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.iter().any(|n| n.ends_with("onClick")),
            "inline template の (click) で参照される method は dead から除外されるべき。got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("reallyUnusedInline")),
            "inline template からも参照されない method は dead として検出されるべき。got: {names:?}"
        );
    }

    /// GitLab issue #8 再現: `@Component` / `@Directive` 装飾クラスの Angular ライフサイクル
    /// フック (`ngAfterViewChecked` 等) は Angular ランタイムが change detection サイクルで
    /// 自動呼出するため、静的解析で caller が見つからなくても dead 判定しない。
    #[test]
    fn detect_dead_excludes_angular_component_lifecycle_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

        let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    template: '<div>example</div>',
})
export class MinimalComponent {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
    public ngOnDestroy(): void {}

    public reallyUnused(): void {}
}
"#;
        fs::write(repo.join("minimal.component.ts"), component_ts).expect("write ts");

        let files = vec![repo.join("minimal.component.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        for hook in ["ngOnInit", "ngAfterViewChecked", "ngOnDestroy"] {
            assert!(
                !names.iter().any(|n| n.ends_with(hook)),
                "Angular @Component の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
            );
        }
        assert!(
            names.iter().any(|n| n.ends_with("reallyUnused")),
            "Angular component の lifecycle hook 以外の未参照 method は引き続き dead として検出されるべき。got: {names:?}"
        );
    }

    /// `@Directive` 装飾クラスでも lifecycle hook を dead から除外する。
    #[test]
    fn detect_dead_excludes_angular_directive_lifecycle_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

        let directive_ts = r#"
import { Directive } from '@angular/core';

@Directive({ selector: '[appFoo]' })
export class FooDirective {
    public ngOnInit(): void {}
    public ngOnChanges(): void {}
}
"#;
        fs::write(repo.join("foo.directive.ts"), directive_ts).expect("write ts");

        let files = vec![repo.join("foo.directive.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        for hook in ["ngOnInit", "ngOnChanges"] {
            assert!(
                !names.iter().any(|n| n.ends_with(hook)),
                "Angular @Directive の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
            );
        }
    }

    /// `@Component` / `@Directive` のいずれも持たないクラスで同名メソッドを定義した場合は
    /// dead から除外せず引き続き検出対象とする (誤除外の防止)。
    #[test]
    fn detect_dead_keeps_non_angular_class_methods_with_lifecycle_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // Angular プロジェクトとして認識されるよう angular.json を置く (誤除外の境界確認)
        fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

        let plain_ts = r#"
export class PlainClass {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
}
"#;
        fs::write(repo.join("plain.ts"), plain_ts).expect("write ts");

        let files = vec![repo.join("plain.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        for hook in ["ngOnInit", "ngAfterViewChecked"] {
            assert!(
                names.iter().any(|n| n.ends_with(hook)),
                "@Component / @Directive を持たないクラスの {hook} は引き続き dead として検出されるべき。got: {names:?}"
            );
        }
    }

    /// 非 Angular プロジェクトでは `.html` ファイルを参照源としてスキャンしない
    /// （誤って HTML 内の単語を参照と誤認しないことの確認）。
    #[test]
    fn detect_dead_does_not_use_html_in_non_angular_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        // angular.json も *.component.ts もない通常の TS プロジェクト
        let ts = r#"
export function ghostHandler(): void {
}
"#;
        fs::write(repo.join("util.ts"), ts).expect("write ts");
        fs::write(
            repo.join("page.html"),
            r#"<button (click)="ghostHandler()">x</button>"#,
        )
        .expect("write html");

        let files = vec![repo.join("util.ts")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

        assert!(
            names.contains(&"ghostHandler"),
            "Angular マーカーが無い場合は HTML 参照を生存判定に使わない (非 Angular なので) 。got: {names:?}"
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
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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

    /// テストディレクトリ配下のシンボル変更は api.add/rm/mod に出さない。
    /// (レポート 2026-04-30-test-symbol-api-detection.md / 2026-04-29-junit-reflection-entrypoints.md の再現)
    /// Tests/ 配下、`*Test.kt`、`*.test.ts` 等のテストファイルは外部 API 面ではない。
    #[test]
    fn detect_api_changes_skips_test_directory_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "package fixture\n\nfun helper() {}\n";
        git_commit_files(repo, &[("app/src/test/java/FooTest.kt", before)], "initial");

        // テスト関数を新規追加
        let after = "package fixture\n\nfun helper() {}\n\
@org.junit.Test\nfun testHelperReturnsZero() {}\n";
        fs::write(repo.join("app/src/test/java/FooTest.kt"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "app/src/test/java/FooTest.kt".to_string(),
            new_path: "app/src/test/java/FooTest.kt".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        let modified: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            added.is_empty(),
            "テストファイル配下の新規シンボルは api.add に出してはならない。got: {added:?}"
        );
        assert!(
            removed.is_empty(),
            "テストファイル配下のシンボル削除は api.rm に出してはならない。got: {removed:?}"
        );
        assert!(
            modified.is_empty(),
            "テストファイル配下のシンボル変更は api.mod に出してはならない。got: {modified:?}"
        );
    }

    /// テストファイル丸ごと削除でも api.rm に出さない。
    /// (Issue D 関連: テストファイルの整理は API 削除ではない)
    #[test]
    fn detect_api_changes_skips_test_file_deletion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "import { describe, it } from 'vitest'\n\
export function testHelper() { return 1 }\n";
        git_commit_files(repo, &[("src/foo.test.ts", before)], "initial");

        std::fs::remove_file(repo.join("src/foo.test.ts")).expect("remove");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/foo.test.ts".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.is_empty(),
            "*.test.ts 削除は api.rm に出してはならない。got: {removed:?}"
        );
    }

    /// JVM/Gradle 標準の `src/test/` 配下は dead-code 検出から既定で除外される。
    /// (レポート 2026-05-21-junit-kotlin-test-dead-symbols.md の再現)
    ///
    /// 2026-04-29 時点の resolved コメントでは「dead 側は既に `test` セグメントで除外済み」と
    /// されていたが、当時の `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` に `test` 単数形は無く、
    /// API 検出側の `is_test_path` のみが `test` を扱っていた。本テストはこのねじれ解消の
    /// 回帰防止: `test` / `androidTest` / `sharedTest` / `integrationTest` セグメントは
    /// 共通定数 `TEST_DIRECTORY_SEGMENTS` 経由で dead-code 側でも既定除外されるべき。
    #[test]
    fn filter_diff_files_for_dead_code_excludes_jvm_src_test_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        std::fs::create_dir_all(repo.join("app/src/test/java/com/example"))
            .expect("mkdir src/test");
        std::fs::write(
            repo.join("app/src/test/java/com/example/FooTest.kt"),
            "package com.example\nclass FooTest\n",
        )
        .expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
            new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];

        let canonical = std::fs::canonicalize(repo).expect("canonicalize");
        // --include-tests なし (既定): DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用
        let excludes = resolve_dead_code_excludes(false, false, false);
        let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
            .expect("filter");

        assert!(
            files.is_empty(),
            "JVM/Gradle 標準の src/test/ 配下は --include-tests なしで dead-code 対象から除外されるべき。got: {files:?}"
        );
    }

    /// `--include-tests` を opt-in した場合は JVM の `src/test/` 配下も走査対象に残る。
    /// (上記 `filter_diff_files_for_dead_code_excludes_jvm_src_test_directory` の対照)
    #[test]
    fn filter_diff_files_for_dead_code_includes_jvm_src_test_directory_when_opted_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        std::fs::create_dir_all(repo.join("app/src/test/java/com/example"))
            .expect("mkdir src/test");
        std::fs::write(
            repo.join("app/src/test/java/com/example/FooTest.kt"),
            "package com.example\nclass FooTest\n",
        )
        .expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
            new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];

        let canonical = std::fs::canonicalize(repo).expect("canonicalize");
        // --include-tests opt-in: DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用しない
        let excludes = resolve_dead_code_excludes(false, true, false);
        let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
            .expect("filter");

        assert_eq!(
            files.len(),
            1,
            "--include-tests 時は src/test/ 配下も走査対象に残るべき。got: {files:?}"
        );
    }

    /// 親ディレクトリ自体に `test` セグメントが含まれていても、root 配下の通常ファイルは
    /// 除外されない。`canonical_dir.join(new_path)` 後の絶対パスを判定材料にしていた
    /// 過去実装では `/private/tmp/test/<repo>/src/lib.rs` が全部除外される false negative
    /// が出た (2026-05-21 codex コミット前レビュー指摘)。除外判定は workspace 相対の
    /// `new_path` で行うべき。
    #[test]
    fn filter_diff_files_for_dead_code_does_not_misclassify_when_ancestor_dir_contains_test_segment()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        // tempdir 直下にさらに "test" セグメントの親ディレクトリを作って、そこにリポを置く
        let repo = dir.path().join("test/myrepo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn existing() {}\npub fn newly_dead() {}\n",
        )
        .expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];

        let canonical = std::fs::canonicalize(&repo).expect("canonicalize");
        let excludes = resolve_dead_code_excludes(false, false, false);
        let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
            .expect("filter");

        assert_eq!(
            files.len(),
            1,
            "親パスが `/.../test/myrepo` でも、リポ内 `src/lib.rs` は除外されないべき。got: {files:?}"
        );
    }

    /// Android instrumentation tests (`src/androidTest/`) も既定除外。
    #[test]
    fn filter_diff_files_for_dead_code_excludes_android_test_source_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        std::fs::create_dir_all(repo.join("app/src/androidTest/java/com/example"))
            .expect("mkdir androidTest");
        std::fs::write(
            repo.join("app/src/androidTest/java/com/example/InstrumentedTest.kt"),
            "package com.example\nclass InstrumentedTest\n",
        )
        .expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
            new_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        }];

        let canonical = std::fs::canonicalize(repo).expect("canonicalize");
        let excludes = resolve_dead_code_excludes(false, false, false);
        let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
            .expect("filter");

        assert!(
            files.is_empty(),
            "Android `src/androidTest/` も既定で dead-code 対象から除外されるべき。got: {files:?}"
        );
    }

    /// TS/JS の constructor は dead 候補から除外される。
    /// (レポート 2026-04-29-typescript-constructor-implicit-call.md の再現)
    /// `new ClassName(...)` で暗黙的に呼ばれるため、`refs --name constructor` で
    /// 見つからず dead 判定される問題への対応。
    #[test]
    fn detect_dead_excludes_typescript_constructor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();

        std::fs::write(
            repo.join("foo.ts"),
            "export class Foo {\n  constructor(public name: string) {}\n  greet() { return this.name; }\n}\n",
        )
        .expect("write");
        std::fs::write(
            repo.join("usage.ts"),
            "import { Foo } from './foo';\nconst f = new Foo('world');\nconsole.log(f.greet());\n",
        )
        .expect("write");

        let candidates =
            extract_dead_code_candidates_from_file(repo.to_str().expect("utf-8 path"), "foo.ts")
                .expect("candidates");
        let names: Vec<&str> = candidates
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with(".constructor") || *n == "constructor"),
            "TS の constructor は dead 候補に含めない。got: {names:?}"
        );
        assert!(
            names.contains(&"Foo"),
            "クラス自体は dead 候補に含まれる。got: {names:?}"
        );
    }

    /// PHP のメソッド名は case-insensitive。case 違い (`isLocalLInk` 定義 / `isLocalLink`
    /// 呼び出し) で参照される public メソッドを dead_symbols に出さない (GitLab #10 の再現)。
    #[test]
    fn detect_dead_php_case_insensitive_method_call_is_not_dead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();

        std::fs::write(
            repo.join("Vo.php"),
            "<?php\nclass Vo {\n    public function isLocalLInk(): bool { return true; }\n}\n",
        )
        .expect("write");
        std::fs::write(
            repo.join("Caller.php"),
            "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isLocalLink(); }\n}\n",
        )
        .expect("write");

        let files = vec![repo.join("Vo.php"), repo.join("Caller.php")];
        let (dead, _test_only) =
            detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
        let dead_names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();
        assert!(
            !dead_names.iter().any(|n| n.ends_with("isLocalLInk")),
            "case 違いで呼ばれる method を dead にしない。got: {dead_names:?}"
        );
    }

    /// React.memo (named function expression) の関数本体内の lexical const は api.add に出さない。
    /// (レポート 2026-05-04-next-page-and-react-memo-false-positives.md パターン1 の再現)
    /// `export const X = memo(function X() { const inner = ... })` の `inner` は
    /// 関数本体スコープのローカル変数で公開 API ではない。`is_js_function_body` の
    /// `function_expression` 認識で境界停止される。
    #[test]
    fn detect_api_changes_excludes_memo_wrapper_internal_const() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();

        std::fs::write(
            repo.join("Card.tsx"),
            "import { memo } from 'react';\n\
export const TaskKanbanCard = memo(function TaskKanbanCard() {\n\
  const hasAssignee = true;\n\
  const milestoneColor = hasAssignee ? 'red' : 'gray';\n\
  return null;\n\
});\n",
        )
        .expect("write");

        let syms =
            extract_exported_symbols_from_file(repo.to_str().expect("utf-8 path"), "Card.tsx")
                .expect("symbols");
        let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"hasAssignee"),
            "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
        );
        assert!(
            !names.contains(&"milestoneColor"),
            "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
        );
        assert!(
            names.contains(&"TaskKanbanCard"),
            "memo で包んだ exported const 自体は API に含める。got: {names:?}"
        );
    }

    /// React.memo ラップで宣言種別が function_declaration → lexical_declaration に変わった
    /// api.mod は、props 型・JSX 利用互換なら compatible (react_component_wrapper) に降格する。
    /// (レポート 2026-06-02-react-memo-api-mod.md の再現)
    #[test]
    fn detect_react_wrapper_jsx_only_usage_is_compatible() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 参照は import + JSX タグのみ (値利用なし)
        std::fs::write(
            dir.path().join("TrayPopup.tsx"),
            "import { ScheduleItem } from './ScheduleItem';\n\
export function TrayPopup() {\n  return <ScheduleItem foo={1} />;\n}\n",
        )
        .expect("write");
        let result = detect_react_wrapper_compatible_mod(
            dir.path().to_str().expect("utf-8 path"),
            "ScheduleItem",
            "ScheduleItem.tsx",
            "function",
            "export function ScheduleItem(props: ScheduleItemProps)",
            "export const ScheduleItem = memo(function ScheduleItem(props: ScheduleItemProps) {",
            Some(crate::language::LangId::Tsx),
        );
        let compat = result.expect("JSX 利用のみ + props 型同一なら compatible");
        assert_eq!(compat.reason, "react_component_wrapper");
        assert_eq!(compat.name, "ScheduleItem");
    }

    /// memo ラップでもシンボルが関数として直接呼び出されている (`X(...)`) 場合は
    /// MemoExoticComponent 化で壊れ得るため blocking (api.mod) を維持する。
    #[test]
    fn detect_react_wrapper_with_call_usage_stays_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("usage.tsx"),
            "import { ScheduleItem } from './ScheduleItem';\n\
const rendered = ScheduleItem({ foo: 1 });\n",
        )
        .expect("write");
        let result = detect_react_wrapper_compatible_mod(
            dir.path().to_str().expect("utf-8 path"),
            "ScheduleItem",
            "ScheduleItem.tsx",
            "function",
            "export function ScheduleItem(props: ScheduleItemProps)",
            "export const ScheduleItem = memo(function ScheduleItem(props: ScheduleItemProps) {",
            Some(crate::language::LangId::Tsx),
        );
        assert!(
            result.is_none(),
            "X(...) 直接呼び出しがあれば blocking 維持 (MemoExoticComponent 非互換)"
        );
    }

    /// props 型が変わった場合は互換でないため blocking を維持する。
    #[test]
    fn detect_react_wrapper_changed_props_type_stays_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("TrayPopup.tsx"),
            "import { ScheduleItem } from './ScheduleItem';\nexport const x = <ScheduleItem />;\n",
        )
        .expect("write");
        let result = detect_react_wrapper_compatible_mod(
            dir.path().to_str().expect("utf-8 path"),
            "ScheduleItem",
            "ScheduleItem.tsx",
            "function",
            "export function ScheduleItem(props: OldProps)",
            "export const ScheduleItem = memo(function ScheduleItem(props: NewProps) {",
            Some(crate::language::LangId::Tsx),
        );
        assert!(result.is_none(), "props 型が変われば blocking 維持");
    }

    #[test]
    fn extract_function_param_list_handles_plain_and_wrapped() {
        assert_eq!(
            extract_function_param_list("export function X(props: T)").as_deref(),
            Some("props: T")
        );
        assert_eq!(
            extract_function_param_list("export const X = memo(function X(props: T) {").as_deref(),
            Some("props: T")
        );
        // arrow function は `function` キーワードが無いので None (保守的に blocking)
        assert_eq!(
            extract_function_param_list("export const X = memo((props: T) => {"),
            None
        );
    }

    #[test]
    fn new_sig_has_react_wrapper_detects_hocs() {
        assert!(new_sig_has_react_wrapper(
            "export const X = memo(function X() {"
        ));
        assert!(new_sig_has_react_wrapper(
            "export const X = React.forwardRef(function X() {"
        ));
        // 単なる function 宣言や部分一致 (somememo) はラッパーでない
        assert!(!new_sig_has_react_wrapper("export function X(props: T)"));
        assert!(!new_sig_has_react_wrapper("export const X = somememo(fn)"));
    }

    #[test]
    fn ctx_usage_classification_jsx_vs_value() {
        // JSX タグ利用は safe
        assert!(ctx_usage_is_jsx_or_safe(
            "return <ScheduleItem foo={1} />;",
            "ScheduleItem"
        ));
        assert!(ctx_usage_is_jsx_or_safe(
            "  </ScheduleItem>",
            "ScheduleItem"
        ));
        // 値利用は blocking 側
        assert!(!ctx_usage_is_jsx_or_safe(
            "const x = ScheduleItem({});",
            "ScheduleItem"
        ));
        assert!(!ctx_usage_is_jsx_or_safe(
            "typeof ScheduleItem",
            "ScheduleItem"
        ));
        assert!(!ctx_usage_is_jsx_or_safe(
            "ScheduleItem.displayName = 'x';",
            "ScheduleItem"
        ));
        // 裸の代入は判定不能なので blocking 側
        assert!(!ctx_usage_is_jsx_or_safe(
            "const Alias = ScheduleItem;",
            "ScheduleItem"
        ));
    }

    /// Bash の未 export 関数を caller ごと同一 diff 内で削除した場合は api.rm に出さない。
    /// (レポート 2026-05-01-bash-private-function-removal-flagged-as-api-rm.md の再現)
    /// `dump_shallow_state` / `boundary_is_old_enough` のように、CLI スクリプト内の
    /// クロージャ的なヘルパー関数を、同 diff 内で全 caller と一緒に削除したとき、
    /// `export -f` が無いなら外部 API 面ではないため除外する必要がある。
    #[test]
    fn detect_api_changes_bash_pure_removal_without_export_is_not_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
dump_shallow_state() {\n    echo state\n}\n\n\
boundary_is_old_enough() {\n    return 0\n}\n\n\
main() {\n    dump_shallow_state\n    while ! boundary_is_old_enough; do\n        sleep 1\n    done\n}\nmain\n";
        git_commit_files(repo, &[("qa_diff.sh", before)], "initial");

        let after = "#!/usr/bin/env bash\n\
main() {\n    echo done\n}\nmain\n";
        fs::write(repo.join("qa_diff.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "qa_diff.sh".to_string(),
            new_path: "qa_diff.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 14,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !removed.contains(&"dump_shallow_state"),
            "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
        );
        assert!(
            !removed.contains(&"boundary_is_old_enough"),
            "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
        );
    }

    /// Bash で `export -f <name>` されている関数の削除は api.rm に残す。
    /// 他リポジトリ消費者向け API として残す必要があるため false negative を避ける。
    #[test]
    fn detect_api_changes_bash_exported_function_removal_is_still_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
public_helper() {\n    echo public\n}\nexport -f public_helper\n\n\
main() {\n    echo hi\n}\nmain\n";
        git_commit_files(repo, &[("lib.sh", before)], "initial");

        let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
        fs::write(repo.join("lib.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "lib.sh".to_string(),
            new_path: "lib.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 8,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"public_helper"),
            "`export -f` された bash 関数の削除は api.rm に残すべき。got: {removed:?}"
        );
    }

    /// Bash の未 export 関数でも、他ファイルから参照されているなら api.rm に残す。
    /// `source common.sh` 経由で他スクリプトが呼ぶケースを考慮し、
    /// cross-file refs が 1 件以上なら除外しない。
    #[test]
    fn detect_api_changes_bash_unexported_function_with_cross_file_ref_is_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = "#!/usr/bin/env bash\n\
shared_helper() {\n    echo shared\n}\n\n\
main() {\n    shared_helper\n}\nmain\n";
        let consumer = "#!/usr/bin/env bash\n\
source ./common.sh\nshared_helper\n";
        git_commit_files(
            repo,
            &[("common.sh", before), ("consumer.sh", consumer)],
            "initial",
        );

        let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
        fs::write(repo.join("common.sh"), after).expect("write");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "common.sh".to_string(),
            new_path: "common.sh".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"shared_helper"),
            "他ファイルから source 経由で参照されている bash 関数の削除は api.rm に残すべき。got: {removed:?}"
        );
    }

    /// Bash スクリプトファイルを丸ごと別言語 (Python) に置き換えた場合、
    /// 未 export な bash 関数は api.rm から除外する。
    /// (レポート 2026-05-01 再発ケース2 / コミット eae0fe0 の再現)
    #[test]
    fn detect_api_changes_bash_file_replaced_with_python_drops_private_funcs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let bash_before = "#!/usr/bin/env bash\n\
fetch_with_retry() {\n    curl \"$1\"\n}\n\n\
main() {\n    fetch_with_retry https://example.com\n}\nmain\n";
        git_commit_files(repo, &[("scripts/qa_diff.sh", bash_before)], "initial");

        // bash スクリプトを削除し、別言語ファイルを新設
        std::fs::remove_file(repo.join("scripts/qa_diff.sh")).expect("remove bash");
        let py_after = "def fetch_with_retry(url: str) -> str:\n    return url\n\n\
def main() -> None:\n    fetch_with_retry(\"https://example.com\")\n\n\
if __name__ == \"__main__\":\n    main()\n";
        fs::write(repo.join("scripts/qa_diff.py"), py_after).expect("write py");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "scripts/qa_diff.sh".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 8,
                    new_start: 0,
                    new_count: 0,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "scripts/qa_diff.py".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 7,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !removed.contains(&"fetch_with_retry"),
            "別言語に置換されたファイル削除でも、未 export bash 関数は api.rm に出してはならない。got: {removed:?}"
        );
    }

    /// Bash ファイル削除時、`export -f` 済み関数は api.rm に残す。
    /// 他リポジトリ消費者向け API として false negative を避ける。
    #[test]
    fn detect_api_changes_bash_file_deletion_keeps_exported_function() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let lib_before = "#!/usr/bin/env bash\n\
public_helper() {\n    echo public\n}\nexport -f public_helper\n";
        git_commit_files(repo, &[("lib.sh", lib_before)], "initial");

        std::fs::remove_file(repo.join("lib.sh")).expect("remove");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "lib.sh".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"public_helper"),
            "ファイル削除でも `export -f` 済み bash 関数は api.rm に残すべき。got: {removed:?}"
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
            deleted_old_source: None,
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
            deleted_old_source: None,
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
        assert!(
            added.contains(&"LibraryApi"),
            "library crate (src/lib.rs あり) の新規 pub struct は api.add に残すべき。got: {added:?}"
        );
    }

    /// 2026-05-19 レポート再現: binary crate (src/lib.rs なし) で `#[allow(dead_code)]`
    /// 付き `pub fn` を削除した場合、直前 hook で `dead` 判定されたシンボルを削除した直後
    /// に同じシンボルが `api.rm` として再警告される矛盾。bin-only crate の `pub fn` は
    /// crate 外から到達できないため、`api.add` 側と対称に `api.rm` 側でも除外する。
    #[test]
    fn detect_api_changes_binary_rust_crate_excludes_pub_removals() {
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
        let executor_before = "\
pub struct RusshExecutor;

impl RusshExecutor {
    pub fn new() -> Self { Self }

    #[allow(dead_code)]
    pub fn with_known_hosts(self, _path: &str) -> Self { self }
}
";
        let main_before = "\
use crate::executor::RusshExecutor;

fn main() {
    let _ = RusshExecutor::new();
}

mod executor;
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/executor.rs", executor_before),
                ("src/main.rs", main_before),
            ],
            "initial",
        );

        // dead 判定済みの `with_known_hosts` を削除する
        let executor_after = "\
pub struct RusshExecutor;

impl RusshExecutor {
    pub fn new() -> Self { Self }
}
";
        fs::write(repo.join("src/executor.rs"), executor_after).expect("write executor");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/executor.rs".to_string(),
            new_path: "src/executor.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 9,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !removed.iter().any(|n| n.ends_with("with_known_hosts")),
            "binary crate (src/lib.rs なし) の pub fn 削除は api.rm に出してはならない。got: {removed:?}"
        );
    }

    /// library crate (src/lib.rs あり) の pub fn 削除は引き続き api.rm に残ること。
    /// binary crate 判定の副作用で library crate の削除まで抑止しないことを保証する。
    #[test]
    fn detect_api_changes_library_rust_crate_keeps_pub_removals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod api;\n";
        let api_before = "\
pub struct Client;

impl Client {
    pub fn new() -> Self { Self }

    pub fn legacy_call(&self) {}
}
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/api.rs", api_before),
            ],
            "initial",
        );

        // 外部公開していた pub fn を削除する
        let api_after = "\
pub struct Client;

impl Client {
    pub fn new() -> Self { Self }
}
";
        fs::write(repo.join("src/api.rs"), api_after).expect("write api");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.iter().any(|n| n.ends_with("legacy_call")),
            "library crate の pub fn 削除は api.rm に残すべき。got: {removed:?}"
        );
    }

    /// 旧ツリーで library crate だったものを同一 diff で `src/lib.rs` 削除 + pub fn 削除に
    /// する場合、`api.rm` は **旧 API 面** の判定なので base 時点の crate type を採用する。
    /// 新ツリーが bin-only に見えても、削除された公開 API は引き続き api.rm に残ること。
    /// (codex pre-commit レビューでの Warning 指摘の回帰テスト)
    #[test]
    fn detect_api_changes_lib_rs_removal_keeps_pub_removals_via_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"was-lib-now-bin\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod api;\n";
        let api_before = "\
pub fn kept() {}
pub fn removed_api() {}
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/api.rs", api_before),
            ],
            "initial",
        );

        // 新ツリーで src/lib.rs を削除し、同時に pub fn removed_api も消す
        std::fs::remove_file(repo.join("src/lib.rs")).expect("rm lib.rs");
        let api_after = "pub fn kept() {}\n";
        fs::write(repo.join("src/api.rs"), api_after).expect("write api");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/lib.rs".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 0,
                    new_count: 0,
                }],
                deleted_old_source: Some(lib_before.as_bytes().to_vec()),
            },
            crate::models::impact::DiffFile {
                old_path: "src/api.rs".to_string(),
                new_path: "src/api.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 1,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.iter().any(|n| n.ends_with("removed_api")),
            "base 時点で library crate だった場合、新ツリーで src/lib.rs を消しても旧公開 API の削除は api.rm に残すべき。got: {removed:?}"
        );
    }

    /// `Cargo.toml` に `[lib] path = "src/api.rs"` のような custom lib path を書いた crate
    /// では `src/lib.rs` が無くても library crate として扱う。`api.rm` 側で誤って公開 API
    /// 削除を抑制しないことを保証する (codex pre-commit レビューでの P1 指摘の回帰テスト)。
    #[test]
    fn detect_api_changes_custom_lib_path_keeps_pub_removals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"custom-lib\"
version = \"0.1.0\"
edition = \"2021\"

[lib]
path = \"src/api.rs\"
";
        let api_before = "\
pub fn kept() {}
pub fn removed_api() {}
";
        git_commit_files(
            repo,
            &[("Cargo.toml", cargo_toml), ("src/api.rs", api_before)],
            "initial",
        );

        let api_after = "pub fn kept() {}\n";
        fs::write(repo.join("src/api.rs"), api_after).expect("write api");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.iter().any(|n| n.ends_with("removed_api")),
            "[lib] path = ... で構成される custom path library crate の pub fn 削除は api.rm に残すべき。got: {removed:?}"
        );
    }

    /// ファイル丸ごと削除のケースでも、binary crate の pub fn は api.rm 対象外にする。
    #[test]
    fn detect_api_changes_binary_rust_crate_excludes_pub_removals_on_file_delete() {
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
        let helper_before = "\
pub fn unused_helper() -> u32 { 42 }
";
        let main_before = "fn main() { println!(\"hi\"); }\nmod helper;\n";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/helper.rs", helper_before),
                ("src/main.rs", main_before),
            ],
            "initial",
        );

        // helper.rs を丸ごと削除
        std::fs::remove_file(repo.join("src/helper.rs")).expect("rm helper");
        let main_after = "fn main() { println!(\"hi\"); }\n";
        fs::write(repo.join("src/main.rs"), main_after).expect("write main");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/helper.rs".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 0,
                    new_count: 0,
                }],
                deleted_old_source: Some(helper_before.as_bytes().to_vec()),
            },
            crate::models::impact::DiffFile {
                old_path: "src/main.rs".to_string(),
                new_path: "src/main.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 1,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api_changes
            .removed
            .iter()
            .chain(api_changes.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !removed.iter().any(|n| n.ends_with("unused_helper")),
            "binary crate のファイル丸ごと削除に含まれる pub fn は api.rm に出してはならない。got: {removed:?}"
        );
    }

    /// 2026-05-20 レポート再現: bin-only crate の `pub fn` シグネチャ変更は外部公開 API の
    /// 互換性問題ではなく内部リファクタなので、`api.mod` 対象外にする (api.add / api.rm と
    /// 対称な動作)。同コミットで caller も更新済みのケース。
    #[test]
    fn detect_api_changes_binary_rust_crate_excludes_pub_method_signature_changes() {
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
        let store_before = "\
pub struct CredentialStore;

impl CredentialStore {
    pub fn get_or_prompt(&mut self, _group: &str, _user: &str, _hint: &str) -> Result<&str, String> {
        Ok(\"password\")
    }
}
";
        let main_before = "\
fn main() {
    use crate::store::CredentialStore;
    let mut s = CredentialStore;
    let _ = s.get_or_prompt(\"g\", \"u\", \"h\");
}

mod store;
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/store.rs", store_before),
                ("src/main.rs", main_before),
            ],
            "initial",
        );

        // シグネチャ変更: 戻り値を `&str` → `(&str, &str)` に拡張、caller も同コミットで追随
        let store_after = "\
pub struct CredentialStore;

impl CredentialStore {
    pub fn get_or_prompt(&mut self, _group: &str, _default_user: &str, _hint: &str) -> Result<(&str, &str), String> {
        Ok((\"user\", \"password\"))
    }
}
";
        let main_after = "\
fn main() {
    use crate::store::CredentialStore;
    let mut s = CredentialStore;
    let _ = s.get_or_prompt(\"g\", \"u\", \"h\");
}

mod store;
";
        fs::write(repo.join("src/store.rs"), store_after).expect("write store");
        fs::write(repo.join("src/main.rs"), main_after).expect("write main");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/store.rs".to_string(),
                new_path: "src/store.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 7,
                    new_start: 1,
                    new_count: 7,
                }],
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "src/main.rs".to_string(),
                new_path: "src/main.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 7,
                    new_start: 1,
                    new_count: 7,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let modified: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !modified.iter().any(|n| n.ends_with("get_or_prompt")),
            "binary crate の pub method シグネチャ変更は api.mod に出してはならない。got: {modified:?}"
        );
    }

    /// library crate (src/lib.rs あり) の pub fn シグネチャ変更は引き続き `api.mod` に残る。
    /// binary crate 判定の副作用で library crate まで抑止しないことを保証する。
    #[test]
    fn detect_api_changes_library_rust_crate_keeps_pub_signature_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod api;\n";
        let api_before = "\
pub fn legacy_call(_x: u32) -> u32 { 0 }
";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/api.rs", api_before),
            ],
            "initial",
        );

        // シグネチャ変更: 引数追加
        let api_after = "\
pub fn legacy_call(_x: u32, _y: u32) -> u32 { 0 }
";
        fs::write(repo.join("src/api.rs"), api_after).expect("write api");

        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let modified: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            modified.iter().any(|n| n.ends_with("legacy_call")),
            "library crate の pub fn シグネチャ変更は引き続き api.mod に残るべき。got: {modified:?}"
        );
    }

    /// base 時点で library crate だったが、新ツリーで `src/lib.rs` を削除して
    /// シグネチャ変更を行ったケース。`api.mod` は「旧版でも新版でも外部公開 API だった
    /// symbol」を対象にすべきなので、旧側基準で library 扱いとなり、新側で bin-only
    /// になっていても旧公開 API のシグネチャ変更は api.mod から除外する
    /// (codex 設計相談で「old または new のどちらかが bin-only なら除外」採用)。
    #[test]
    fn detect_api_changes_lib_to_bin_transition_excludes_pub_signature_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let cargo_toml = "\
[package]
name = \"was-lib-now-bin\"
version = \"0.1.0\"
edition = \"2021\"
";
        let lib_before = "pub mod api;\n";
        let api_before = "pub fn frob(_x: u32) -> u32 { 0 }\n";
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", cargo_toml),
                ("src/lib.rs", lib_before),
                ("src/api.rs", api_before),
            ],
            "initial",
        );

        // 新ツリーで src/lib.rs を削除 + シグネチャ変更
        std::fs::remove_file(repo.join("src/lib.rs")).expect("rm lib.rs");
        fs::write(
            repo.join("src/api.rs"),
            "pub fn frob(_x: u32, _y: u32) -> u32 { 0 }\n",
        )
        .expect("write api");

        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "src/lib.rs".to_string(),
                new_path: "/dev/null".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 0,
                    new_count: 0,
                }],
                deleted_old_source: Some(lib_before.as_bytes().to_vec()),
            },
            crate::models::impact::DiffFile {
                old_path: "src/api.rs".to_string(),
                new_path: "src/api.rs".to_string(),
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                }],
                deleted_old_source: None,
            },
        ];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let modified: Vec<&str> = api_changes
            .modified
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !modified.iter().any(|n| n.ends_with("frob")),
            "lib → bin 化 + シグネチャ変更のケースは api.mod に出さない (crate target 変更として扱う)。got: {modified:?}"
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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
    // is_bash_script_path / bash_has_export_f ヘルパー
    // ------------------------------------------------------------------

    #[test]
    fn is_bash_script_path_matches_shell_extensions() {
        assert!(is_bash_script_path("scripts/foo.sh"));
        assert!(is_bash_script_path("scripts/foo.bash"));
        assert!(is_bash_script_path("scripts/foo.zsh"));
        assert!(!is_bash_script_path("scripts/foo.py"));
        assert!(!is_bash_script_path("scripts/Makefile"));
        assert!(!is_bash_script_path("scripts/foo"));
    }

    #[test]
    fn bash_has_export_f_detects_export_minus_f() {
        let src = "#!/usr/bin/env bash\n\
foo() { echo hi; }\n\
export -f foo\n\
bar() { echo bye; }\n";
        assert!(bash_has_export_f(src, "foo"));
        assert!(!bash_has_export_f(src, "bar"));
    }

    #[test]
    fn bash_has_export_f_detects_declare_variants() {
        let src = "    declare -fx foo\n  declare -xf bar\n";
        assert!(bash_has_export_f(src, "foo"));
        assert!(bash_has_export_f(src, "bar"));
    }

    #[test]
    fn bash_has_export_f_supports_multiple_names_per_line() {
        let src = "export -f foo bar baz\n";
        assert!(bash_has_export_f(src, "foo"));
        assert!(bash_has_export_f(src, "bar"));
        assert!(bash_has_export_f(src, "baz"));
        assert!(!bash_has_export_f(src, "qux"));
    }

    #[test]
    fn bash_has_export_f_does_not_match_partial_or_substring() {
        let src = "export -f foo_bar\n";
        assert!(bash_has_export_f(src, "foo_bar"));
        assert!(!bash_has_export_f(src, "foo"));
        assert!(!bash_has_export_f(src, "bar"));
    }

    #[test]
    fn bash_has_export_f_rejects_empty_name() {
        let src = "export -f \n";
        assert!(!bash_has_export_f(src, ""));
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

    /// `Cargo.toml` に `[lib] path = "..."` を書いて `src/lib.rs` を使わず custom path で
    /// library crate を構成しているケース。`src/lib.rs` の有無だけ見ると binary-only と
    /// 誤判定し、本物の公開 API 削除を `api.rm` から除外してしまうため、`[lib]` セクション
    /// 存在を判定要件に含める。
    #[test]
    fn is_binary_only_rust_crate_false_when_cargo_lib_section_with_custom_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        fs::create_dir_all(repo.join("src")).expect("mkdir src");
        let cargo_toml = "[package]\nname = \"custom\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/api.rs\"\n";
        fs::write(repo.join("Cargo.toml"), cargo_toml).expect("cargo");
        fs::write(repo.join("src/api.rs"), "pub fn hello() {}\n").expect("api");

        assert!(!is_binary_only_rust_crate(
            repo.to_str().expect("utf-8"),
            "src/api.rs",
        ));
    }

    // ------------------------------------------------------------------
    // cargo_toml_text_declares_lib ヘルパー
    // ------------------------------------------------------------------

    #[test]
    fn cargo_toml_text_declares_lib_true_when_lib_section_present() {
        let text = "[package]\nname = \"x\"\n\n[lib]\npath = \"src/api.rs\"\n";
        assert!(cargo_toml_text_declares_lib(text));
    }

    #[test]
    fn cargo_toml_text_declares_lib_false_when_lib_section_absent() {
        let text = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n";
        assert!(!cargo_toml_text_declares_lib(text));
    }

    #[test]
    fn cargo_toml_text_declares_lib_false_when_empty() {
        // 空 TOML は library 宣言なし
        assert!(!cargo_toml_text_declares_lib(""));
    }

    /// 不正な TOML は `api.rm` の見逃しを避けるため保守的に true (= library 扱い) を返す。
    #[test]
    fn cargo_toml_text_declares_lib_true_when_invalid_toml() {
        let text = "this is = not valid = toml\n[unclosed";
        assert!(cargo_toml_text_declares_lib(text));
    }

    /// `[[bin]]` セクションだけがあって `[lib]` がない場合は binary-only として扱う。
    #[test]
    fn cargo_toml_text_declares_lib_false_when_only_bin_array_section() {
        let text = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"x\"\npath = \"src/main.rs\"\n";
        assert!(!cargo_toml_text_declares_lib(text));
    }

    // ------------------------------------------------------------------
    // auto_detect_framework ヘルパー
    // ------------------------------------------------------------------

    #[test]
    fn auto_detect_framework_returns_none_without_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
    }

    #[test]
    fn auto_detect_framework_returns_nextjs_for_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies": {"next": "14.0.0", "react": "18.0.0"}}"#,
        )
        .expect("pkg");
        assert_eq!(
            auto_detect_framework(dir.path().to_str().expect("utf-8")),
            Some("nextjs")
        );
    }

    #[test]
    fn auto_detect_framework_returns_nextjs_for_dev_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"devDependencies": {"next": "14.0.0"}}"#,
        )
        .expect("pkg");
        assert_eq!(
            auto_detect_framework(dir.path().to_str().expect("utf-8")),
            Some("nextjs")
        );
    }

    /// `peerDependencies` / `optionalDependencies` 経由の `next` は library 側の同梱で
    /// 誤爆しやすいため対象外とする。
    #[test]
    fn auto_detect_framework_ignores_peer_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"peerDependencies": {"next": "14.0.0"}}"#,
        )
        .expect("pkg");
        assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
    }

    #[test]
    fn auto_detect_framework_returns_none_for_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("package.json"), "{not valid json").expect("pkg");
        assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
    }

    #[test]
    fn auto_detect_framework_returns_none_when_no_next_dependency() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies": {"react": "18.0.0"}}"#,
        )
        .expect("pkg");
        assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
    }

    /// `resolve_framework_globs_with_auto_detect`: 明示指定があれば auto detect は無視する。
    #[test]
    fn resolve_framework_globs_with_auto_detect_prefers_explicit_framework() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 明示指定が `laravel` のとき、package.json に next があっても laravel プリセットを返す。
        fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies": {"next": "14.0.0"}}"#,
        )
        .expect("pkg");
        let globs = resolve_framework_globs_with_auto_detect(
            Some("laravel"),
            dir.path().to_str().expect("utf-8"),
        )
        .expect("resolve");
        // Laravel プリセットの代表 glob `**/app/Http/**` が含まれていることだけ確認する。
        assert!(globs.iter().any(|g| g.contains("Http")));
    }

    /// auto detect 経由でも明示指定無し時は nextjs プリセットが返ること。
    #[test]
    fn resolve_framework_globs_with_auto_detect_uses_auto_when_no_explicit() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies": {"next": "14.0.0"}}"#,
        )
        .expect("pkg");
        let globs =
            resolve_framework_globs_with_auto_detect(None, dir.path().to_str().expect("utf-8"))
                .expect("resolve");
        // nextjs プリセットの代表 glob `**/app/**` または `**/pages/**` のどちらかが含まれる。
        assert!(
            globs
                .iter()
                .any(|g| g.contains("app/**") || g.contains("pages/**"))
        );
    }

    /// package.json も `--framework` も無いケースは空 Vec を返す (Ok(Vec::new()))。
    #[test]
    fn resolve_framework_globs_with_auto_detect_empty_when_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let globs =
            resolve_framework_globs_with_auto_detect(None, dir.path().to_str().expect("utf-8"))
                .expect("resolve");
        assert!(globs.is_empty());
    }

    #[test]
    fn reconcile_with_moves_pairs_by_signature() {
        // reconcile_with_moves のユニットテスト: 同じ (name,kind,sig) を相殺して
        // moved に分類し、残りだけを返す。
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
        let all_new_candidates = added.clone();

        let (kept_added, kept_removed, moved) =
            reconcile_with_moves(added, removed, all_new_candidates);
        assert_eq!(kept_added.len(), 1);
        assert_eq!(kept_added[0].name, "new_api");
        assert_eq!(kept_removed.len(), 1);
        assert_eq!(kept_removed[0].name, "gone");
        assert_eq!(moved.len(), 1, "同シグネチャは moved に集約される");
        assert_eq!(moved[0].name, "foo");
        assert_eq!(moved[0].from, "old.py");
        assert_eq!(moved[0].to, "new.py");
    }

    #[test]
    fn reconcile_with_moves_keeps_different_signatures() {
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
        let all_new_candidates = added.clone();

        let (kept_added, kept_removed, moved) =
            reconcile_with_moves(added, removed, all_new_candidates);
        assert_eq!(kept_added.len(), 1);
        assert_eq!(kept_removed.len(), 1);
        assert!(
            moved.is_empty(),
            "シグネチャが違えば moved に乗らない。got: {moved:?}"
        );
    }

    #[test]
    fn reconcile_with_moves_uses_filtered_new_candidates_for_pairing() {
        // is_used_in_diff_paths などで `added` から落ちた候補も all_new_candidates
        // に残っていれば removed と相殺する。module → package 化リファクタの中核。
        let added: Vec<ApiSymbolCandidate> = Vec::new();
        let removed = vec![ApiSymbolCandidate {
            name: "rotate_command".into(),
            kind: "function".into(),
            file: "src/cli.py".into(),
            signature: "def rotate_command(name: str):".into(),
        }];
        let all_new_candidates = vec![ApiSymbolCandidate {
            name: "rotate_command".into(),
            kind: "function".into(),
            file: "src/cli/_commands/rotate.py".into(),
            signature: "def rotate_command(name: str):".into(),
        }];

        let (kept_added, kept_removed, moved) =
            reconcile_with_moves(added, removed, all_new_candidates);
        assert!(
            kept_added.is_empty(),
            "added に乗らないので残らない: {kept_added:?}"
        );
        assert!(
            kept_removed.is_empty(),
            "all_new_candidates と組めば removed から消える: {kept_removed:?}"
        );
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].name, "rotate_command");
        assert_eq!(moved[0].from, "src/cli.py");
        assert_eq!(moved[0].to, "src/cli/_commands/rotate.py");
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
            deleted_old_source: None,
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
            &[
                (
                    "src/old.rs",
                    "pub fn greet() -> i32 {\n    1\n}\n\npub fn farewell() -> i32 {\n    0\n}\n",
                ),
                (
                    // caller を別ファイルに置いて farewell を参照させる (rename 削除でも
                    // removed_dead ではなく removed として残ることを確認するため)
                    "src/caller.rs",
                    "pub fn use_farewell() -> i32 { crate::farewell() }\n",
                ),
            ],
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
            deleted_old_source: None,
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
                deleted_old_source: None,
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
                deleted_old_source: None,
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
            false,
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
                skipped: None,
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
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
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
                skipped: None,
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
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
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
                skipped: None,
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
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
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
                skipped: None,
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
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
        assert!(build.value.is_some(), "api.mod は hook JSON に出すべき");
        assert!(build.is_blocking, "api.mod は blocking にすべき");
    }

    #[test]
    fn build_review_hook_json_const_value_only_is_informational() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: vec![ApiSymbolChange {
                    name: "ENEMY_SPEED".to_string(),
                    kind: "constant".to_string(),
                    file: "src/constants.rs".to_string(),
                    old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                    new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                }],
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };
        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
        assert!(
            build.value.is_some(),
            "const_value 変更は informational として hook JSON に出すべき"
        );
        assert!(
            !build.is_blocking,
            "const_value のみの変更はデフォルトで blocking にしないべき"
        );
    }

    #[test]
    fn build_review_hook_json_const_value_is_blocking_under_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: Vec::new(),
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: vec![ApiSymbolChange {
                    name: "ENEMY_SPEED".to_string(),
                    kind: "constant".to_string(),
                    file: "src/constants.rs".to_string(),
                    old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                    new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                }],
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };
        let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), true);
        assert!(
            build.is_blocking,
            "--strict-public-const-values 指定時は const_value を blocking に昇格すべき"
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
                        // caller.symbols は「この caller が参照している、変更ファイル内の
                        // シンボル名」(pass3.rs::build_file_impact の構築意図)。
                        // 呼び出し元関数の名前は ImpactedCaller.name 側に入る。
                        symbols: vec!["compute".to_string()],
                        confidence: None,
                    }],
                    low_confidence_callers: Vec::new(),
                }],
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
        let hook_json = build.value.expect("hook json should be generated");
        assert!(build.is_blocking, "impacts があれば blocking にすべき");
        let impacts = hook_json["impacts"]
            .as_array()
            .expect("impacts should be an array");
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0]["src"], "src/lib.rs");
        assert_eq!(impacts[0]["syms"], serde_json::json!(["compute"]));
        assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["compute"]));
    }

    /// hook の `syms` には cross-file caller を発生させた causal symbol だけを残し、
    /// 非 export const や本体未変更の export を除外する (Issue 2026-05-14
    /// private-const-and-unchanged-export-noise)。
    #[test]
    fn build_review_hook_json_filters_non_causal_affected_symbols_from_syms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::write(src_dir.join("a.rs"), "pub fn foo() {}\n").expect("write changed file");
        fs::write(src_dir.join("b.rs"), "fn caller() { foo(); }\n").expect("write caller");

        // affected_symbols は変更ファイル内で hunk と overlap した全シンボル。
        // PRIVATE_CONST と unchanged_export は cross-file 検索で is_symbol_exported に
        // 弾かれて caller.symbols には含まれないため、hook の syms にも出てはならない。
        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: vec![crate::models::impact::FileImpact {
                    path: "src/a.rs".to_string(),
                    hunks: Vec::new(),
                    affected_symbols: vec![
                        crate::models::impact::AffectedSymbol {
                            name: "foo".to_string(),
                            kind: "function".to_string(),
                            change_type: "modified".to_string(),
                        },
                        crate::models::impact::AffectedSymbol {
                            name: "PRIVATE_CONST".to_string(),
                            kind: "constant".to_string(),
                            change_type: "modified".to_string(),
                        },
                        crate::models::impact::AffectedSymbol {
                            name: "unchanged_export".to_string(),
                            kind: "function".to_string(),
                            change_type: "modified".to_string(),
                        },
                    ],
                    signature_changes: Vec::new(),
                    impacted_callers: vec![crate::models::impact::ImpactedCaller {
                        path: "src/b.rs".to_string(),
                        name: "caller".to_string(),
                        line: 1,
                        symbols: vec!["foo".to_string()],
                        confidence: None,
                    }],
                    low_confidence_callers: Vec::new(),
                }],
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build =
            build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
        let hook_json = build.value.expect("hook json should be generated");
        assert!(build.is_blocking, "未解決 impact があれば blocking");
        let impacts = hook_json["impacts"]
            .as_array()
            .expect("impacts should be an array");
        assert_eq!(impacts.len(), 1);
        assert_eq!(
            impacts[0]["syms"],
            serde_json::json!(["foo"]),
            "syms は cross-file caller を発生させた causal symbol だけになるべき (PRIVATE_CONST と unchanged_export は除外)"
        );
        // refs[].s は元々 caller.symbols そのまま (causal の絞り込みは不要)
        assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["foo"]));
    }

    /// 新規追加 (`change_type=added`) シンボルへの caller のみがある場合、
    /// hook blocking には含めない。同コミットで新規シンボルと新規参照が
    /// セットで導入されるのは自然な依存関係で、breaking change ではない
    /// (Issue 2026-05-27-added-symbol-initial-reference)。
    #[test]
    fn build_review_hook_json_added_only_caller_is_not_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::write(src_dir.join("constants.rs"), "pub const FOO: u32 = 1;\n").unwrap();
        fs::write(
            src_dir.join("user.rs"),
            "use crate::constants::FOO; fn x() { let _ = FOO; }\n",
        )
        .unwrap();

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: vec![crate::models::impact::FileImpact {
                    path: "src/constants.rs".to_string(),
                    hunks: Vec::new(),
                    affected_symbols: vec![crate::models::impact::AffectedSymbol {
                        name: "FOO".to_string(),
                        kind: "constant".to_string(),
                        change_type: "added".to_string(),
                    }],
                    signature_changes: Vec::new(),
                    impacted_callers: vec![crate::models::impact::ImpactedCaller {
                        path: "src/user.rs".to_string(),
                        name: "x".to_string(),
                        line: 1,
                        symbols: vec!["FOO".to_string()],
                        confidence: None,
                    }],
                    low_confidence_callers: Vec::new(),
                }],
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
        // 新規追加シンボルへの caller のみ → impacts は空 (blocking 対象外)
        assert!(
            build.value.is_none() || {
                let v = build.value.as_ref().unwrap();
                v.get("impacts")
                    .and_then(|i| i.as_array())
                    .is_none_or(|a| a.is_empty())
            },
            "added シンボルのみへの caller は hook impacts から除外されるべき: {:?}",
            build.value
        );
        assert!(
            !build.is_blocking,
            "added のみの場合は Stop hook を止めないべき"
        );
    }

    /// 同 caller が added と modified の両方を参照している場合、modified だけを
    /// causal symbol として残し blocking する。
    #[test]
    fn build_review_hook_json_mixed_added_and_modified_keeps_only_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::write(
            src_dir.join("a.rs"),
            "pub fn modified_fn() {}\npub const NEW_CONST: u32 = 1;\n",
        )
        .unwrap();
        fs::write(
            src_dir.join("b.rs"),
            "use crate::a::{modified_fn, NEW_CONST}; fn caller() { modified_fn(); let _ = NEW_CONST; }\n",
        )
        .unwrap();

        let result = ReviewResult {
            impact: crate::models::impact::ContextResult {
                changes: vec![crate::models::impact::FileImpact {
                    path: "src/a.rs".to_string(),
                    hunks: Vec::new(),
                    affected_symbols: vec![
                        crate::models::impact::AffectedSymbol {
                            name: "modified_fn".to_string(),
                            kind: "function".to_string(),
                            change_type: "modified".to_string(),
                        },
                        crate::models::impact::AffectedSymbol {
                            name: "NEW_CONST".to_string(),
                            kind: "constant".to_string(),
                            change_type: "added".to_string(),
                        },
                    ],
                    signature_changes: Vec::new(),
                    impacted_callers: vec![crate::models::impact::ImpactedCaller {
                        path: "src/b.rs".to_string(),
                        name: "caller".to_string(),
                        line: 1,
                        symbols: vec!["modified_fn".to_string(), "NEW_CONST".to_string()],
                        confidence: None,
                    }],
                    low_confidence_callers: Vec::new(),
                }],
                skipped: None,
            },
            missing_cochanges: Vec::new(),
            api_changes: ApiChanges {
                added: Vec::new(),
                removed: Vec::new(),
                modified: Vec::new(),
                moved: Vec::new(),
                property_to_field: Vec::new(),
                removed_dead: Vec::new(),
                modified_closed_in_diff: Vec::new(),
                const_value_changes: Vec::new(),
                compatible_modified: Vec::new(),
            },
            dead_symbols: Vec::new(),
            test_only_symbols: Vec::new(),
            skipped: None,
        };

        let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
        let hook_json = build.value.expect("hook json should be generated");
        assert!(build.is_blocking, "modified を含むため blocking");
        let impacts = hook_json["impacts"].as_array().expect("impacts array");
        assert_eq!(impacts.len(), 1);
        // syms / refs[].s には modified_fn のみが残り、NEW_CONST (added) は落ちる
        assert_eq!(
            impacts[0]["syms"],
            serde_json::json!(["modified_fn"]),
            "added 由来の NEW_CONST は syms から除外され modified_fn のみ残るべき"
        );
        assert_eq!(
            impacts[0]["refs"][0]["s"],
            serde_json::json!(["modified_fn"]),
            "refs[].s も modified_fn のみに絞られるべき"
        );
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
            None,
        )
        .expect("detect_missing_cochanges should succeed");

        assert!(
            missing.iter().all(|m| m.file != "Cargo.toml"),
            "Cargo.toml が missing_cochange に含まれてはならない。got: {missing:?}"
        );
    }

    #[test]
    fn detect_missing_cochanges_uses_review_base_for_multi_commit_ranges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(
            repo,
            &[
                (
                    "a.rs",
                    "fn a() {\n    let first = 0;\n    let second = 0;\n}\n",
                ),
                (
                    "b.rs",
                    "fn b() {\n    let first = 0;\n    let second = 0;\n}\n",
                ),
            ],
            "initial",
        );
        git_commit_files(
            repo,
            &[
                (
                    "a.rs",
                    "fn a() {\n    let first = 1;\n    let second = 0;\n}\n",
                ),
                (
                    "b.rs",
                    "fn b() {\n    let first = 1;\n    let second = 0;\n}\n",
                ),
            ],
            "pair 1",
        );
        git_commit_files(
            repo,
            &[
                (
                    "a.rs",
                    "fn a() {\n    let first = 1;\n    let second = 2;\n}\n",
                ),
                (
                    "b.rs",
                    "fn b() {\n    let first = 1;\n    let second = 2;\n}\n",
                ),
            ],
            "pair 2",
        );
        git_commit_files(
            repo,
            &[(
                "a.rs",
                "fn a() {\n    let first = 10;\n    let second = 2;\n}\n",
            )],
            "a only 1",
        );
        git_commit_files(
            repo,
            &[(
                "a.rs",
                "fn a() {\n    let first = 10;\n    let second = 20;\n}\n",
            )],
            "a only 2",
        );

        let service = AppService::new();
        let mut changed_files = HashSet::new();
        changed_files.insert("a.rs".to_string());

        // 小サンプル (co=2, denom=2) なので新デフォルト β=8 では
        // score=(2+1)/(2+1+8)=0.27 となり、production の min_confidence=0.3
        // からは弾かれる。本テストは「base が blame 解析に正しく渡る」を
        // 確かめるのが目的なので、閾値を 0.0 に下げて信号の有無だけ見る。
        let missing = detect_missing_cochanges(
            &service,
            repo.to_str().expect("utf-8 path"),
            &changed_files,
            0.0,
            Some("HEAD~2"),
        )
        .expect("detect_missing_cochanges should succeed");

        assert!(
            missing.iter().any(|m| m.file == "b.rs"),
            "review の base が blame 解析に渡らず HEAD~1 のみを見ると b.rs を見落とす。got: {missing:?}"
        );
    }

    /// review の detect_missing_cochanges が cochange 入力検証エラーを silent に握り潰さず
    /// 呼び出し側へ伝播することを確認する回帰テスト。
    #[test]
    fn detect_missing_cochanges_propagates_invalid_request_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(repo, &[("a.rs", "v1")], "initial");

        let service = AppService::new();
        let mut changed_files = HashSet::new();
        changed_files.insert("a.rs".to_string());

        // NaN は AppService::analyze_cochange の入力検証で InvalidRequest を返すため、
        // detect_missing_cochanges もそのエラーを伝播するはず。
        let result = detect_missing_cochanges(
            &service,
            repo.to_str().expect("utf-8 path"),
            &changed_files,
            f64::NAN,
            None,
        );

        let err = result.expect_err("NaN min_confidence should surface as error");
        let astro_err = err
            .downcast_ref::<crate::error::AstroError>()
            .expect("expect AstroError");
        assert_eq!(astro_err.code, crate::error::ErrorCode::InvalidRequest);
    }

    #[test]
    fn resolve_blame_source_files_filters_default_excludes_for_git_only() {
        // --git 経由で diff から起点ファイルを取得する場合、
        // BLAME_DEFAULT_EXCLUDE_GLOBS に該当する生成物 (dist/, *.lock) は除外される。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(repo, &[("foo.txt", "v1")], "initial");
        git_commit_files(
            repo,
            &[
                ("foo.txt", "v2"),
                ("dist/main.js", "minified"),
                ("Cargo.lock", "lockfile"),
                ("Angular/www/dist/bundle.js", "minified"),
            ],
            "next",
        );

        let BlameSourceResolution::Files(result) = resolve_blame_source_files(
            repo.to_str().expect("utf-8 path"),
            true,
            Some("HEAD~1"),
            None,
            None,
            &[],
        )
        .expect("resolve") else {
            panic!("expected Files");
        };

        assert!(result.contains(&"foo.txt".to_string()), "got: {result:?}");
        assert!(
            !result.iter().any(|p| p == "dist/main.js"),
            "dist/main.js は BLAME_DEFAULT_EXCLUDE_GLOBS で除外されるはず。got: {result:?}"
        );
        assert!(
            !result.iter().any(|p| p == "Cargo.lock"),
            "Cargo.lock は除外されるはず。got: {result:?}"
        );
        assert!(
            !result.iter().any(|p| p == "Angular/www/dist/bundle.js"),
            "サブディレクトリの dist/ も除外されるはず。got: {result:?}"
        );
    }

    #[test]
    fn resolve_blame_source_files_keeps_explicit_paths_unfiltered() {
        // --paths で明示指定した起点はユーザー意図を尊重し、
        // BLAME_DEFAULT_EXCLUDE_GLOBS 該当でも除外しない。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(repo, &[("dummy.txt", "x")], "initial");

        let BlameSourceResolution::Files(result) = resolve_blame_source_files(
            repo.to_str().expect("utf-8 path"),
            false,
            None,
            Some("dist/main.js,Cargo.lock"),
            None,
            &[],
        )
        .expect("resolve") else {
            panic!("expected Files");
        };

        assert!(result.contains(&"dist/main.js".to_string()));
        assert!(result.contains(&"Cargo.lock".to_string()));
    }

    #[test]
    fn resolve_blame_source_files_applies_user_exclude_glob_for_git() {
        // --git 経由のとき --exclude-glob (user_exclude_globs) も適用される。
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        git_commit_files(repo, &[("foo.txt", "v1")], "initial");
        git_commit_files(
            repo,
            &[
                ("foo.txt", "v2"),
                ("legacy/keep.rs", "old"),
                ("generated/codegen.rs", "auto"),
            ],
            "next",
        );

        let BlameSourceResolution::Files(result) = resolve_blame_source_files(
            repo.to_str().expect("utf-8 path"),
            true,
            Some("HEAD~1"),
            None,
            None,
            &["generated/**".to_string()],
        )
        .expect("resolve") else {
            panic!("expected Files");
        };

        assert!(result.contains(&"foo.txt".to_string()));
        assert!(result.contains(&"legacy/keep.rs".to_string()));
        assert!(
            !result.iter().any(|p| p == "generated/codegen.rs"),
            "ユーザー指定 --exclude-glob は --git 経由の起点に適用される。got: {result:?}"
        );
    }

    /// dead-code --glob が positive whitelist として絞り込みに使われていることを確認する。
    /// 以前は Match::None も許可されており、`**/*.py` 指定でも Rust ファイル等が残っていた。
    #[test]
    fn filter_diff_files_for_dead_code_glob_acts_as_whitelist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        // glob による絞り込みの単体検証なので、実ファイルは作らず diff 模擬のみ。
        let diff_files = vec![
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/foo.rs".to_string(),
                hunks: Vec::new(),
                deleted_old_source: None,
            },
            crate::models::impact::DiffFile {
                old_path: "/dev/null".to_string(),
                new_path: "src/bar.py".to_string(),
                hunks: Vec::new(),
                deleted_old_source: None,
            },
        ];

        let files = filter_diff_files_for_dead_code(repo, &diff_files, &[], &[], Some("**/*.py"))
            .expect("filter");

        // glob 絞り込み後は Python ファイルだけが残るべき。
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "bar.py"),
            "py ファイルは glob に一致するため残る。got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "foo.rs"),
            "rs ファイルは glob にマッチしないため除外される。got: {names:?}"
        );
    }

    /// detect_api_changes は diff path のトラバーサルを安全に無視する。
    /// `../etc/passwd` のような diff を渡しても workspace 外を読まない。
    #[test]
    fn detect_api_changes_skips_unsafe_diff_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let dir_str = repo.to_str().expect("utf-8 path");

        let unsafe_diff = vec![crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "../etc/passwd".to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        }];

        // パス検証で弾かれ、added/removed/modified ともに空配列を返すこと。
        let result = detect_api_changes(dir_str, "HEAD", &unsafe_diff);
        assert!(result.added.is_empty());
        assert!(result.removed.is_empty());
        assert!(result.modified.is_empty());
    }
}
