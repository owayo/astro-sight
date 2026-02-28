use crate::error::AstroError;
use camino::Utf8Path;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LangId {
    Rust,
    C,
    Cpp,
    Python,
    Javascript,
    Typescript,
    Tsx,
    Go,
    Php,
    Java,
    Kotlin,
    Swift,
    #[serde(rename = "csharp")]
    CSharp,
    Bash,
}

impl std::fmt::Display for LangId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Rust => "rust",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Python => "python",
            Self::Javascript => "javascript",
            Self::Typescript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Php => "php",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::CSharp => "csharp",
            Self::Bash => "bash",
        };
        write!(f, "{s}")
    }
}

// tree-sitter-kotlin 0.3.5 uses old tree-sitter API; bridge via extern C function
#[link(name = "parser", kind = "static")]
unsafe extern "C" {
    safe fn tree_sitter_kotlin() -> *const std::ffi::c_void;
}

impl LangId {
    /// Detect language from file extension.
    pub fn from_path(path: &Utf8Path) -> Result<Self, AstroError> {
        let ext = path.extension().unwrap_or("").to_lowercase();

        match ext.as_str() {
            "rs" => Ok(Self::Rust),
            "c" | "h" => Ok(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Ok(Self::Cpp),
            "py" | "pyi" => Ok(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Ok(Self::Javascript),
            "ts" | "mts" | "cts" => Ok(Self::Typescript),
            "tsx" => Ok(Self::Tsx),
            "go" => Ok(Self::Go),
            "php" | "phtml" => Ok(Self::Php),
            "java" => Ok(Self::Java),
            "kt" | "kts" => Ok(Self::Kotlin),
            "swift" => Ok(Self::Swift),
            "cs" => Ok(Self::CSharp),
            "sh" | "bash" | "zsh" => Ok(Self::Bash),
            other => {
                if other.is_empty() {
                    Err(AstroError::unsupported_language("<no extension>"))
                } else {
                    Err(AstroError::unsupported_language(other))
                }
            }
        }
    }

    /// Detect language from shebang line.
    pub fn from_shebang(first_line: &str) -> Option<Self> {
        if !first_line.starts_with("#!") {
            return None;
        }
        let line = first_line.to_lowercase();
        if line.contains("python") {
            Some(Self::Python)
        } else if line.contains("node") || line.contains("deno") || line.contains("bun") {
            Some(Self::Javascript)
        } else if line.contains("php") {
            Some(Self::Php)
        } else if line.contains("kotlin") {
            Some(Self::Kotlin)
        } else if line.contains("swift") {
            Some(Self::Swift)
        } else if line.contains("bash") || line.contains("/sh") || line.contains("zsh") {
            Some(Self::Bash)
        } else {
            None
        }
    }

    /// Get the tree-sitter Language for this language ID.
    pub fn ts_language(self) -> Language {
        match self {
            Self::Rust => Language::new(tree_sitter_rust::LANGUAGE),
            Self::C => Language::new(tree_sitter_c::LANGUAGE),
            Self::Cpp => Language::new(tree_sitter_cpp::LANGUAGE),
            Self::Python => Language::new(tree_sitter_python::LANGUAGE),
            Self::Javascript => Language::new(tree_sitter_javascript::LANGUAGE),
            Self::Typescript => Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            Self::Tsx => Language::new(tree_sitter_typescript::LANGUAGE_TSX),
            Self::Go => Language::new(tree_sitter_go::LANGUAGE),
            Self::Php => Language::new(tree_sitter_php::LANGUAGE_PHP),
            Self::Java => Language::new(tree_sitter_java::LANGUAGE),
            Self::Kotlin => {
                let ptr = tree_sitter_kotlin().cast::<tree_sitter::ffi::TSLanguage>();
                unsafe { Language::from_raw(ptr) }
            }
            Self::Swift => Language::new(tree_sitter_swift::LANGUAGE),
            Self::CSharp => Language::new(tree_sitter_c_sharp::LANGUAGE),
            Self::Bash => Language::new(tree_sitter_bash::LANGUAGE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_rust() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("src/main.rs")).unwrap(),
            LangId::Rust
        );
    }

    #[test]
    fn detect_c() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("main.c")).unwrap(),
            LangId::C
        );
    }

    #[test]
    fn detect_typescript() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("app.ts")).unwrap(),
            LangId::Typescript
        );
    }

    #[test]
    fn detect_tsx() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("component.tsx")).unwrap(),
            LangId::Tsx
        );
    }

    #[test]
    fn detect_php() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("index.php")).unwrap(),
            LangId::Php
        );
    }

    #[test]
    fn detect_java() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("Main.java")).unwrap(),
            LangId::Java
        );
    }

    #[test]
    fn detect_kotlin() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("App.kt")).unwrap(),
            LangId::Kotlin
        );
    }

    #[test]
    fn detect_swift() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("main.swift")).unwrap(),
            LangId::Swift
        );
    }

    #[test]
    fn detect_csharp() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("Program.cs")).unwrap(),
            LangId::CSharp
        );
    }

    #[test]
    fn detect_bash() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("deploy.sh")).unwrap(),
            LangId::Bash
        );
    }

    #[test]
    fn detect_unsupported() {
        assert!(LangId::from_path(Utf8Path::new("file.xyz")).is_err());
    }

    #[test]
    fn shebang_python() {
        assert_eq!(
            LangId::from_shebang("#!/usr/bin/env python3"),
            Some(LangId::Python)
        );
    }

    #[test]
    fn shebang_bash() {
        assert_eq!(LangId::from_shebang("#!/bin/bash"), Some(LangId::Bash));
    }
}
