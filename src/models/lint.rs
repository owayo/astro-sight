use serde::{Deserialize, Serialize};

/// Severity level for a lint rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A lint rule definition (loaded from YAML).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Unique rule identifier
    pub id: String,
    /// Target language (e.g. "rust", "javascript")
    pub language: String,
    /// Severity level
    pub severity: Severity,
    /// Human-readable message
    pub message: String,
    /// tree-sitter S-expression query (mutually exclusive with pattern)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Simple text pattern to match against identifiers (mutually exclusive with query)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A single pattern match result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternMatch {
    /// Rule ID that matched
    pub rule_id: String,
    /// Severity
    pub severity: Severity,
    /// Message
    pub message: String,
    /// Line number (0-indexed)
    pub line: usize,
    /// Column number (0-indexed)
    pub column: usize,
    /// Matched text
    pub matched_text: String,
}

/// Result of linting a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub matches: Vec<PatternMatch>,
    /// Warnings about skipped or invalid rules
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
