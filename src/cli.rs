use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "astro-sight",
    version,
    about = "AI-agent-friendly AST information generation CLI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Pretty-print JSON output (default: compact)
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Enable debug logging
    #[arg(long, global = true)]
    pub debug: bool,

    /// Path to configuration file
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Extract AST fragment at a given position or range
    Ast {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,

        /// Line number (0-indexed)
        #[arg(short, long)]
        line: Option<usize>,

        /// Column number (0-indexed)
        #[arg(short, long)]
        col: Option<usize>,

        /// End line (for range extraction)
        #[arg(long)]
        end_line: Option<usize>,

        /// End column (for range extraction)
        #[arg(long)]
        end_col: Option<usize>,

        /// Max depth of AST traversal (default: 3)
        #[arg(short, long, default_value = "3")]
        depth: usize,

        /// Number of context lines in snippet (default: 3)
        #[arg(long, default_value = "3")]
        context: usize,

        /// Full output with id, named, nested range (legacy format)
        #[arg(long)]
        full: bool,

        /// Disable cache
        #[arg(long)]
        no_cache: bool,
    },

    /// Extract symbols from a source file
    Symbols {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,

        /// Directory to scan for source files (NDJSON output)
        #[arg(long, conflicts_with_all = ["path", "paths", "paths_file"])]
        dir: Option<String>,

        /// Glob pattern to filter files when using --dir (e.g. "**/*.rs")
        #[arg(long)]
        glob: Option<String>,

        /// Custom tree-sitter query
        #[arg(short, long)]
        query: Option<String>,

        /// Include docstrings in compact output
        #[arg(long, conflicts_with = "full")]
        doc: bool,

        /// Full output with hash, range, and doc (legacy format)
        #[arg(long)]
        full: bool,

        /// Disable cache
        #[arg(long)]
        no_cache: bool,
    },

    /// Extract call graph from a source file
    Calls {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,

        /// Filter to a specific function name
        #[arg(short, long)]
        function: Option<String>,
    },

    /// Search for symbol references across files
    Refs {
        /// Symbol name to search for (single mode)
        #[arg(short, long)]
        name: Option<String>,

        /// Comma-separated symbol names (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "name")]
        names: Option<String>,

        /// Directory to search in
        #[arg(short, long)]
        dir: String,

        /// Glob pattern to filter files (e.g. "**/*.rs")
        #[arg(short, long)]
        glob: Option<String>,
    },

    /// Smart context: analyze diff impact
    Context {
        /// Workspace directory
        #[arg(short, long)]
        dir: String,

        /// Inline diff string
        #[arg(long)]
        diff: Option<String>,

        /// Path to a diff file
        #[arg(long, conflicts_with = "diff")]
        diff_file: Option<String>,

        /// Auto-run git diff to get changes
        #[arg(long, conflicts_with_all = ["diff", "diff_file"])]
        git: bool,

        /// Base ref for git diff (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,

        /// Use staged changes (git diff --cached)
        #[arg(long)]
        staged: bool,
    },

    /// Detect unresolved change impacts (for stop hooks)
    Impact {
        /// Workspace directory
        #[arg(short, long)]
        dir: String,

        /// Auto-run git diff to get changes
        #[arg(long)]
        git: bool,

        /// Base ref for git diff (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,

        /// Use staged changes (git diff --cached)
        #[arg(long)]
        staged: bool,

        /// Append triage hint for AI agent hooks
        #[arg(long)]
        hook: bool,
    },

    /// Extract import/export dependencies from a source file
    Imports {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,
    },

    /// Lint source files with AST pattern rules
    Lint {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,

        /// Path to YAML rules file
        #[arg(short, long)]
        rules: Option<String>,

        /// Directory containing YAML rule files
        #[arg(long, conflicts_with = "rules")]
        rules_dir: Option<String>,
    },

    /// Generate Mermaid sequence diagram from call graph
    Sequence {
        /// Path to the source file (single mode)
        #[arg(short, long)]
        path: Option<String>,

        /// Comma-separated paths (batch mode, NDJSON output)
        #[arg(long, conflicts_with = "path")]
        paths: Option<String>,

        /// File containing paths, one per line (batch mode)
        #[arg(long, conflicts_with_all = ["path", "paths"])]
        paths_file: Option<String>,

        /// Filter to a specific function name
        #[arg(short, long)]
        function: Option<String>,
    },

    /// Analyze blame-based co-change patterns for a diff or specified source files.
    Cochange {
        /// Git repository directory
        #[arg(short, long, default_value = ".")]
        dir: String,

        /// Use git diff to derive source files
        #[arg(long)]
        git: bool,

        /// Base revision for diff/blame (default: HEAD~1)
        #[arg(long)]
        base: Option<String>,

        /// Comma-separated source file paths (relative to repo root)
        #[arg(long)]
        paths: Option<String>,

        /// File containing one source path per line
        #[arg(long)]
        paths_file: Option<String>,

        /// Minimum confidence threshold (0.0 to 1.0). Default: 0.3.
        #[arg(short, long, default_value = "0.3")]
        min_confidence: f64,

        /// Minimum shared commit count required per pair (default: 2)
        #[arg(long, default_value = "2")]
        min_samples: usize,

        /// Skip co-change counting for commits touching more files than this
        /// threshold (default: 100; hard cap, the size weighting below
        /// (`--commit-size-pivot`) handles softer suppression).
        #[arg(long, default_value = "100")]
        max_files_per_commit: usize,

        /// Commit-size weighting pivot. Each commit gets weight
        /// `min(1.0, sqrt(pivot/file_count))` when computing the smoothed
        /// `score`, so large refactor commits contribute less than focused
        /// commits. `0` disables weighting (legacy behaviour). Default: 8.
        #[arg(long, default_value = "8")]
        commit_size_pivot: usize,

        /// Exclude candidate paths matching this glob (repeatable).
        /// Built-in defaults already exclude vendor/, node_modules/, lock files, minified assets.
        #[arg(long = "exclude-glob")]
        exclude_globs: Vec<String>,

        /// Maximum number of source files allowed (0 = unlimited).
        /// Exceeding this limit aborts with InvalidRequest to prevent runaway blame cost.
        #[arg(long, default_value = "0")]
        max_source_files: usize,

        /// Track file rename/move via `git blame -M`.
        /// Slightly slower but recovers history across rename boundaries.
        #[arg(long)]
        rename: bool,

        /// Detect file copy via `git blame -C` (heavier than `--rename`).
        /// Useful for repositories with frequent copy-paste / file-split refactors.
        #[arg(long)]
        copy: bool,

        /// Drop merge commits from the blame commit set before counting co-changes.
        /// Useful when the repository has many squash-merge style merges that bloat diff-tree output.
        #[arg(long)]
        ignore_merges: bool,

        /// Maximum number of blame commits allowed in the SHA set (0 = unlimited).
        /// Defends against pathological blame fan-out by aborting before the diff-tree phase.
        #[arg(long, default_value = "0")]
        max_blame_commits: usize,

        /// Overall timeout in seconds (0 = unlimited).
        /// Checked at each phase entry; in-flight subprocesses are not killed
        /// (the most recent invocation completes before the timeout fires).
        #[arg(long, default_value = "0")]
        timeout_secs: u64,

        /// Disable Bayesian smoothing.
        /// By default smoothing is enabled to suppress small-sample over-confidence
        /// (e.g. co=1/denom=1 yielding 1.00). Use this flag to fall back to raw co/denom ranking.
        #[arg(long)]
        no_smoothing: bool,

        /// Bayesian smoothing alpha (success prior, default 1.0).
        /// score = (co + alpha) / (denom + alpha + beta).
        #[arg(long, default_value = "1.0")]
        smoothing_alpha: f64,

        /// Bayesian smoothing beta (failure prior, default 8.0).
        /// Higher beta penalises small denominators more strongly so
        /// `co=2/denom=2` no longer dominates the ranking.
        #[arg(long, default_value = "8.0")]
        smoothing_beta: f64,

        /// Skip source files whose blame commit set is smaller than this.
        /// 0/1 = disabled (legacy behaviour). Default 2.
        #[arg(long, default_value = "2")]
        min_denominator: usize,

        /// Limit candidates per source file to top N (0 = unlimited).
        /// Default 10 to keep output focused.
        #[arg(long, default_value = "10")]
        per_source_limit: usize,
    },

    /// Structured review: integrates impact, cochange, API surface diff, and dead symbol detection
    Review {
        /// Workspace directory
        #[arg(short, long)]
        dir: String,

        /// Inline diff string
        #[arg(long)]
        diff: Option<String>,

        /// Path to a diff file
        #[arg(long, conflicts_with = "diff")]
        diff_file: Option<String>,

        /// Auto-run git diff to get changes
        #[arg(long, conflicts_with_all = ["diff", "diff_file"])]
        git: bool,

        /// Base ref for git diff (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,

        /// Use staged changes (git diff --cached)
        #[arg(long)]
        staged: bool,

        /// Minimum cochange confidence threshold (0.0 to 1.0, default: 0.7)
        #[arg(long, default_value = "0.7")]
        min_confidence: f64,

        /// Append triage hint for AI agent hooks
        #[arg(long)]
        hook: bool,

        /// Framework preset を指定して dead_symbols からフレームワーク規約の
        /// エントリポイントを除外する。現在対応: "laravel" (database/migrations,
        /// app/Http/Controllers, app/Http/Middleware, app/Providers 等)
        #[arg(long)]
        framework: Option<String>,

        /// dead_symbols 検出時に追加で除外するディレクトリ名 (完全一致、複数指定可)。
        /// 例: --exclude-dir generated --exclude-dir .cache
        #[arg(long = "exclude-dir", value_name = "NAME", num_args = 0..)]
        exclude_dirs: Vec<String>,

        /// dead_symbols 検出時に追加で除外する glob パターン (ワークスペース相対、複数指定可)。
        /// 先頭の `!` は不要 (内部で negative pattern として扱う)。
        /// 例: --exclude-glob 'app/Legacy/**' --exclude-glob 'config/*.php'
        #[arg(long = "exclude-glob", value_name = "PATTERN", num_args = 0..)]
        exclude_globs: Vec<String>,
    },

    /// Detect dead (unreferenced) exported symbols
    DeadCode {
        /// Workspace / project root directory
        #[arg(short, long, default_value = ".")]
        dir: String,

        /// Glob pattern to filter files (e.g. "**/*.rs")
        #[arg(short, long)]
        glob: Option<String>,

        /// Inline diff string (limit scan to diff-related files only)
        #[arg(long)]
        diff: Option<String>,

        /// Path to a diff file (limit scan to diff-related files only)
        #[arg(long, conflicts_with = "diff")]
        diff_file: Option<String>,

        /// Auto-run git diff (limit scan to diff-related files only)
        #[arg(long, conflicts_with_all = ["diff", "diff_file"])]
        git: bool,

        /// Base ref for git diff (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,

        /// Use staged changes (git diff --cached)
        #[arg(long)]
        staged: bool,

        /// Include vendor / node_modules / .venv 等のパッケージマネージャ配下
        /// (既定: 除外)
        #[arg(long)]
        include_vendor: bool,

        /// Include tests / Tests / __tests__ / spec / testdata ディレクトリ配下
        /// (既定: 除外)
        #[arg(long)]
        include_tests: bool,

        /// Include target / dist / build / out 等のビルド成果物ディレクトリ配下
        /// (既定: 除外)
        #[arg(long)]
        include_build: bool,

        /// Framework preset を指定してフレームワーク規約のエントリポイントを除外する。
        /// 現在対応: "laravel" (database/migrations, app/Http/Controllers 等)
        #[arg(long)]
        framework: Option<String>,

        /// 追加で除外するディレクトリ名 (完全一致、複数指定可)。
        /// 例: --exclude-dir generated --exclude-dir .cache
        #[arg(long = "exclude-dir", value_name = "NAME", num_args = 0..)]
        exclude_dirs: Vec<String>,

        /// 追加で除外する glob パターン (ワークスペース相対、複数指定可)。
        /// 先頭の `!` は不要 (内部で negative pattern として扱う)。
        /// 例: --exclude-glob 'app/Legacy/**' --exclude-glob 'config/*.php'
        #[arg(long = "exclude-glob", value_name = "PATTERN", num_args = 0..)]
        exclude_globs: Vec<String>,
    },

    /// Check tool availability and language support
    Doctor,

    /// NDJSON streaming session (stdin → stdout)
    Session,

    /// Start MCP (Model Context Protocol) server over stdio
    Mcp,

    /// Generate default configuration file
    Init {
        /// Path to write the configuration file (default: ~/.config/astro-sight/config.toml)
        #[arg(short, long)]
        path: Option<std::path::PathBuf>,
    },

    /// Install astro-sight skill for an AI agent
    SkillInstall {
        /// Target agent: "claude" (~/.claude/skills/) or "codex" (~/.codex/skills/)
        target: String,
    },
}
