use serde::{Deserialize, Serialize};

/// The kind of a reference (definition or usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    #[serde(rename = "def")]
    Definition,
    #[serde(rename = "ref")]
    Reference,
}

/// A single reference to a symbol in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub path: String,
    #[serde(rename = "ln")]
    pub line: usize,
    #[serde(rename = "col")]
    pub column: usize,
    #[serde(rename = "ctx", skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefKind>,
}

/// The refs response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsResult {
    pub symbol: String,
    #[serde(rename = "refs")]
    pub references: Vec<SymbolReference>,
}
