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

#[cfg(test)]
mod tests {
    use super::*;

    /// 各 ErrorCode の Display が正しい文字列を返すことを検証
    #[test]
    fn error_code_display() {
        assert_eq!(ErrorCode::FileNotFound.to_string(), "FILE_NOT_FOUND");
        assert_eq!(ErrorCode::ParseError.to_string(), "PARSE_ERROR");
        assert_eq!(ErrorCode::InvalidRange.to_string(), "INVALID_RANGE");
        assert_eq!(
            ErrorCode::UnsupportedLanguage.to_string(),
            "UNSUPPORTED_LANGUAGE"
        );
        assert_eq!(ErrorCode::CacheError.to_string(), "CACHE_ERROR");
        assert_eq!(ErrorCode::IoError.to_string(), "IO_ERROR");
        assert_eq!(ErrorCode::InvalidRequest.to_string(), "INVALID_REQUEST");
        assert_eq!(ErrorCode::PathOutOfBounds.to_string(), "PATH_OUT_OF_BOUNDS");
    }

    /// AstroError の Display が "[CODE] message" 形式であることを検証
    #[test]
    fn astro_error_display() {
        let err = AstroError::new(ErrorCode::IoError, "test msg");
        assert_eq!(err.to_string(), "[IO_ERROR] test msg");
    }

    /// file_not_found ヘルパーが正しいコードとメッセージを返すことを検証
    #[test]
    fn astro_error_file_not_found() {
        let err = AstroError::file_not_found("/path/to/file");
        assert_eq!(err.code, ErrorCode::FileNotFound);
        assert!(err.message.contains("/path/to/file"));
    }

    /// unsupported_language ヘルパーが正しいコードとメッセージを返すことを検証
    #[test]
    fn astro_error_unsupported_language() {
        let err = AstroError::unsupported_language(".xyz");
        assert_eq!(err.code, ErrorCode::UnsupportedLanguage);
        assert!(err.message.contains(".xyz"));
    }

    /// parse_error ヘルパーが正しいコードとメッセージを返すことを検証
    #[test]
    fn astro_error_parse_error() {
        let err = AstroError::parse_error("/some/file.rs");
        assert_eq!(err.code, ErrorCode::ParseError);
        assert!(err.message.contains("/some/file.rs"));
    }
}
