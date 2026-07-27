use super::location::Range;
use serde::{Deserialize, Serialize};

/// AST ノードの表現。
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

/// AST の親から子へのエッジ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstEdge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub node: AstNode,
}

// ── compact（トークン最適化）形式 ──

/// トークン最適化した AST ノード。id/named を省き、
/// range は [startLine, startCol, endLine, endCol] で表す。
/// 出力を平坦化するため、旧 CompactAstEdge の field をインライン化している。
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
