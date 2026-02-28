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
    /// Symbol name to search (for refs command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Directory to search in (for refs/context commands)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Glob pattern filter (for refs command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Diff input (for context command via session)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
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
}
