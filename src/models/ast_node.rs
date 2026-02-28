use super::location::Range;
use serde::{Deserialize, Serialize};

/// An AST node representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstNode {
    pub id: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named: Option<bool>,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<AstEdge>,
}

/// An edge from parent to child in the AST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstEdge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub node: AstNode,
}
