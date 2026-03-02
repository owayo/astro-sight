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

// ── Compact (token-optimized) variants ──

/// Token-optimized AST node: no id/named, range as [startLine, startCol, endLine, endCol].
/// field is inlined from the former CompactAstEdge for flatter output.
#[derive(Debug, Clone, Serialize)]
pub struct CompactAstNode {
    pub kind: String,
    pub range: [usize; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<CompactAstNode>,
}

impl AstNode {
    pub fn to_compact(&self) -> CompactAstNode {
        self.to_compact_with_field(None)
    }

    fn to_compact_with_field(&self, field: Option<String>) -> CompactAstNode {
        CompactAstNode {
            kind: self.kind.clone(),
            range: [
                self.range.start.line,
                self.range.start.column,
                self.range.end.line,
                self.range.end.column,
            ],
            field,
            text: self.text.clone(),
            children: self
                .children
                .iter()
                .map(|e| e.node.to_compact_with_field(e.field.clone()))
                .collect(),
        }
    }
}
