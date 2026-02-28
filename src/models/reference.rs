use serde::{Deserialize, Serialize};

/// The kind of a reference (definition or usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Definition,
    Reference,
}

/// A single reference to a symbol in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub path: String,
    pub line: usize,
    pub column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefKind>,
}

/// The refs response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsResult {
    pub version: String,
    pub symbol: String,
    pub references: Vec<SymbolReference>,
}
