use super::ast_node::AstNode;
use super::diagnostic::Diagnostic;
use super::location::LocationKey;
use super::symbol::Symbol;
use crate::error::ErrorCode;
use crate::language::LangId;
use serde::{Deserialize, Serialize};

/// The response envelope for all commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstgenResponse {
    pub version: String,
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
            version: env!("CARGO_PKG_VERSION").to_string(),
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
            version: env!("CARGO_PKG_VERSION").to_string(),
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
}
