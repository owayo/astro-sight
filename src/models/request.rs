use serde::{Deserialize, Serialize};

/// A request to the astro-sight engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstgenRequest {
    pub command: Command,
    #[serde(default)]
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Function name filter (for calls command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Symbol name to search (for refs command, single mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Symbol names for batch refs search
    #[serde(skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// Directory to search in (for refs/context commands)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Glob pattern filter (for refs command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Diff input (for context command via session)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Lint rules (for lint command via session)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<crate::models::lint::Rule>>,
    /// Minimum confidence for co-change analysis (blame mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// Minimum shared commit count for co-change analysis (blame mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_samples: Option<usize>,
    /// Commits touching more files than this threshold are skipped (blame mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_files_per_commit: Option<usize>,
    /// Source files for blame-based co-change analysis (relative to repo root)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_files: Option<Vec<String>>,
    /// Base revision for blame-based co-change analysis (defaults to HEAD~1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// 追加で除外するディレクトリ名 (context コマンドの impact cross-file 解析で適用)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_dirs: Vec<String>,
    /// 追加で除外する glob パターン (context コマンドの impact cross-file 解析で適用)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_globs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Ast,
    Symbols,
    Doctor,
    Calls,
    Refs,
    Context,
    Imports,
    Lint,
    Sequence,
    Cochange,
}
