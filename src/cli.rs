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

#[derive(Subcommand)]
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

        /// Custom tree-sitter query
        #[arg(short, long)]
        query: Option<String>,

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
        /// Symbol name to search for
        #[arg(short, long)]
        name: String,

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
