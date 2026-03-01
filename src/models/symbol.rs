use super::location::Range;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Variable,
    Constant,
    Module,
    Import,
    Type,
    Field,
    Parameter,
}

/// A symbol definition extracted from source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<Symbol>,
}

/// Compact symbol for token-optimized output.
#[derive(Debug, Clone, Serialize)]
pub struct CompactSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<CompactSymbol>,
}

impl Symbol {
    pub fn to_compact(&self, include_doc: bool) -> CompactSymbol {
        CompactSymbol {
            name: self.name.clone(),
            kind: self.kind,
            line: self.range.start.line,
            doc: if include_doc { self.doc.clone() } else { None },
            children: self
                .children
                .iter()
                .map(|c| c.to_compact(include_doc))
                .collect(),
        }
    }
}

/// A reference to a symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}
