use super::ast_node::{AstNode, CompactAstNode};
use super::diagnostic::Diagnostic;
use super::location::LocationKey;
use super::symbol::{CompactSymbol, Symbol};
use crate::error::ErrorCode;
use crate::language::LangId;
use serde::{Deserialize, Serialize};

/// The response envelope for all commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstgenResponse {
    pub location: LocationKey,
    pub language: LangId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ast: Option<Vec<AstNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Vec<Symbol>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
}

impl AstgenResponse {
    pub fn success(location: LocationKey, language: LangId) -> Self {
        Self {
            location,
            language,
            hash: None,
            ast: None,
            symbols: None,
            snippet: None,
            diagnostics: Vec::new(),
            error: None,
        }
    }

    pub fn error(location: LocationKey, language: LangId, code: ErrorCode, msg: &str) -> Self {
        Self {
            location,
            language,
            hash: None,
            ast: None,
            symbols: None,
            snippet: None,
            diagnostics: Vec::new(),
            error: Some(ErrorInfo {
                code,
                message: msg.to_string(),
            }),
        }
    }

    pub fn to_compact_ast(&self) -> CompactAstResponse {
        CompactAstResponse {
            location: self.location.clone(),
            language: self.language,
            schema: AstSchema::default(),
            ast: self
                .ast
                .as_ref()
                .map(|nodes| nodes.iter().map(|n| n.to_compact()).collect())
                .unwrap_or_default(),
            diagnostics: self.diagnostics.clone(),
        }
    }

    pub fn to_compact_symbols(&self, include_doc: bool) -> CompactSymbolsResponse {
        CompactSymbolsResponse {
            location: self.location.clone(),
            language: self.language,
            symbols: self
                .symbols
                .as_ref()
                .map(|syms| syms.iter().map(|s| s.to_compact(include_doc)).collect())
                .unwrap_or_default(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

/// Schema hint for compact AST output.
#[derive(Debug, Clone, Serialize)]
pub struct AstSchema {
    pub range: &'static str,
}

impl Default for AstSchema {
    fn default() -> Self {
        Self {
            range: "[startLine,startCol,endLine,endCol]",
        }
    }
}

/// Token-optimized response for ast command.
#[derive(Debug, Clone, Serialize)]
pub struct CompactAstResponse {
    pub location: LocationKey,
    pub language: LangId,
    pub schema: AstSchema,
    pub ast: Vec<CompactAstNode>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// Token-optimized response for symbols command.
#[derive(Debug, Clone, Serialize)]
pub struct CompactSymbolsResponse {
    pub location: LocationKey,
    pub language: LangId,
    pub symbols: Vec<CompactSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}
