use serde::{Deserialize, Serialize};

/// The kind of import statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    Import,
    Use,
    Include,
    Require,
}

/// A single import edge extracted from source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEdge {
    /// The imported module/path/package
    #[serde(rename = "src")]
    pub source: String,
    /// Line number (0-indexed)
    #[serde(rename = "ln")]
    pub line: usize,
    /// Kind of import
    pub kind: ImportKind,
    /// The source text of the import statement
    #[serde(rename = "ctx")]
    pub context: String,
}

/// Result of import extraction for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportsResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub imports: Vec<ImportEdge>,
}
