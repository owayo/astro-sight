use std::sync::Arc;

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
///
/// `path` は `Arc<str>` で、同一ファイル内の大量参照でパス文字列を共有する。
/// これによりヒープ割当数が 1 ファイルあたり 1 回に収まり、数万参照の
/// ピーク RSS を大幅に削減する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub path: Arc<str>,
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
