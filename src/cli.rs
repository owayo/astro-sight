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

    /// Analyze co-change patterns from git history
    Cochange {
        /// Git repository directory
        #[arg(short, long, default_value = ".")]
        dir: String,

        /// Number of recent commits to analyze (default: 200)
        #[arg(short, long, default_value = "200")]
        lookback: usize,

        /// Minimum confidence threshold (0.0 to 1.0, default: 0.3)
        #[arg(short, long, default_value = "0.3")]
        min_confidence: f64,

        /// Filter to pairs containing this file
        #[arg(short, long)]
        file: Option<String>,
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
