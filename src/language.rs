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
    Ruby,
    Zig,
    Xojo,
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
            Self::Ruby => "ruby",
            Self::Zig => "zig",
            Self::Xojo => "xojo",
        };
        write!(f, "{s}")
    }
}

// tree-sitter-kotlin 0.3.5 uses old tree-sitter API; bridge via extern C function
// `safe fn` は Rust 2024 構文だが tree-sitter-rust がパースできないため、
// 旧来の unsafe extern + ラッパー関数で対応
mod ffi_kotlin {
    #[link(name = "parser", kind = "static")]
    unsafe extern "C" {
        pub unsafe fn tree_sitter_kotlin() -> *const std::ffi::c_void;
    }
}

fn tree_sitter_kotlin() -> *const std::ffi::c_void {
    unsafe { ffi_kotlin::tree_sitter_kotlin() }
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
            "rb" | "rake" | "gemspec" => Ok(Self::Ruby),
            "zig" | "zon" => Ok(Self::Zig),
            "xojo_code" | "xojo_window" | "xojo_menu" | "xojo_toolbar" | "xojo_report"
            | "rbbas" => Ok(Self::Xojo),
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
        } else if line.contains("bash")
            || line.contains("/bin/sh")
            || line.contains("env sh")
            || line.contains("zsh")
        {
            Some(Self::Bash)
        } else if line.contains("ruby") {
            Some(Self::Ruby)
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
            Self::Ruby => Language::new(tree_sitter_ruby::LANGUAGE),
            Self::Zig => Language::new(tree_sitter_zig::LANGUAGE),
            Self::Xojo => Language::new(tree_sitter_xojo::LANGUAGE),
        }
    }

    /// 識別子の大文字小文字を区別しない言語かどうかを返す。
    /// Xojo は仕様上 `myVar` と `MYVAR` を同一識別子として扱う。
    #[inline]
    pub fn is_case_insensitive(self) -> bool {
        matches!(self, Self::Xojo)
    }
}

/// case-insensitive な言語では識別子を Unicode-aware に小文字化する。
/// 非 CI 言語ではゼロコピーで `Cow::Borrowed` を返し、既存の挙動を変えない。
pub fn normalize_identifier(lang: LangId, name: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !lang.is_case_insensitive() {
        return Cow::Borrowed(name);
    }
    if name.is_ascii() {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Owned(name.chars().flat_map(|c| c.to_lowercase()).collect())
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
    fn detect_ruby() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("app.rb")).unwrap(),
            LangId::Ruby
        );
    }

    #[test]
    fn detect_zig() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("main.zig")).unwrap(),
            LangId::Zig
        );
    }

    #[test]
    fn detect_xojo_code() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("Window1.xojo_code")).unwrap(),
            LangId::Xojo
        );
    }

    #[test]
    fn detect_xojo_window() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("Form.xojo_window")).unwrap(),
            LangId::Xojo
        );
    }

    #[test]
    fn detect_xojo_rbbas() {
        assert_eq!(
            LangId::from_path(Utf8Path::new("Legacy.rbbas")).unwrap(),
            LangId::Xojo
        );
    }

    #[test]
    fn case_insensitive_only_xojo() {
        assert!(LangId::Xojo.is_case_insensitive());
        assert!(!LangId::Rust.is_case_insensitive());
        assert!(!LangId::Python.is_case_insensitive());
        assert!(!LangId::Ruby.is_case_insensitive());
    }

    #[test]
    fn normalize_identifier_xojo_ascii() {
        let v = normalize_identifier(LangId::Xojo, "MyVar");
        assert_eq!(&*v, "myvar");
    }

    #[test]
    fn normalize_identifier_non_ci_is_zero_copy() {
        let v = normalize_identifier(LangId::Rust, "MyVar");
        assert!(matches!(v, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*v, "MyVar");
    }

    #[test]
    fn normalize_identifier_xojo_unicode() {
        // Xojo は Unicode 識別子 (日本語メンバ名) をサポート
        let v = normalize_identifier(LangId::Xojo, "顧客名");
        assert_eq!(&*v, "顧客名");
        // ß はドイツ語の eszett — to_lowercase で "ss" ではなく "ß" のまま残る
        let v = normalize_identifier(LangId::Xojo, "Größe");
        assert_eq!(&*v, "größe");
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

    #[test]
    fn shebang_bin_sh() {
        assert_eq!(LangId::from_shebang("#!/bin/sh"), Some(LangId::Bash));
    }

    #[test]
    fn shebang_env_sh() {
        assert_eq!(
            LangId::from_shebang("#!/usr/bin/env sh"),
            Some(LangId::Bash)
        );
    }

    #[test]
    fn shebang_zsh() {
        assert_eq!(LangId::from_shebang("#!/bin/zsh"), Some(LangId::Bash));
    }

    /// `/sh` を含むが sh ではないコマンド（ssh 等）は Bash として誤検出しない
    #[test]
    fn shebang_ssh_not_bash() {
        assert_eq!(LangId::from_shebang("#!/usr/bin/ssh"), None);
    }
}
