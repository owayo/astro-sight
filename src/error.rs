use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    FileNotFound,
    ParseError,
    InvalidRange,
    UnsupportedLanguage,
    CacheError,
    IoError,
    InvalidRequest,
    PathOutOfBounds,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotFound => write!(f, "FILE_NOT_FOUND"),
            Self::ParseError => write!(f, "PARSE_ERROR"),
            Self::InvalidRange => write!(f, "INVALID_RANGE"),
            Self::UnsupportedLanguage => write!(f, "UNSUPPORTED_LANGUAGE"),
            Self::CacheError => write!(f, "CACHE_ERROR"),
            Self::IoError => write!(f, "IO_ERROR"),
            Self::InvalidRequest => write!(f, "INVALID_REQUEST"),
            Self::PathOutOfBounds => write!(f, "PATH_OUT_OF_BOUNDS"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AstroError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for AstroError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AstroError {}

impl AstroError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn file_not_found(path: &str) -> Self {
        Self::new(ErrorCode::FileNotFound, format!("File not found: {path}"))
    }

    pub fn unsupported_language(ext: &str) -> Self {
        Self::new(
            ErrorCode::UnsupportedLanguage,
            format!("Unsupported language for extension: {ext}"),
        )
    }

    pub fn parse_error(path: &str) -> Self {
        Self::new(ErrorCode::ParseError, format!("Failed to parse: {path}"))
    }
}
