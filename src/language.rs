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
        // 表示名は DetectedLang 側に一本化し二重管理を避ける
        write!(f, "{}", self.detected().display_name())
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

    /// 拡張子からの言語検出に失敗した場合、source の先頭行 shebang を見て再検出する。
    /// CLI 系コマンドのエントリポイント (`extract_ast` / `extract_symbols` 等) が
    /// 同じ振る舞いを必要とするため、共通ヘルパーとして公開する。
    pub fn detect(path: &Utf8Path, source: &[u8]) -> Result<Self, AstroError> {
        Self::from_path(path).or_else(|_| {
            let first_line = std::str::from_utf8(source)
                .ok()
                .and_then(|s| s.lines().next())
                .unwrap_or("");
            Self::from_shebang(first_line).ok_or_else(|| {
                AstroError::unsupported_language(path.extension().unwrap_or("<none>"))
            })
        })
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
    ///
    /// # Panics
    /// `is_lexer_only()` が true の言語に対しては panic する。呼び出し前に
    /// 必ず `is_lexer_only()` で振り分けるか、`detected().tree_sitter()` を
    /// 使うこと。PR2 で Xojo を lexer-only に格下げしたためこの保証を導入した。
    pub fn ts_language(self) -> Language {
        // 実体は TreeSitterLang::ts_language に委譲し、言語 arm の二重メンテを避ける。
        // lexer-only (Xojo) は tree-sitter を持たないため従来どおり panic する。
        match self.detected().tree_sitter() {
            Some(lang) => lang.ts_language(),
            None => {
                panic!("Xojo is lexer-only since v26.6; callers must check is_lexer_only() first")
            }
        }
    }

    /// 識別子の大文字小文字を区別しない言語かどうかを返す。
    /// Xojo は仕様上 `myVar` と `MYVAR` を同一識別子として扱う。
    #[inline]
    pub fn is_case_insensitive(self) -> bool {
        matches!(self, Self::Xojo)
    }

    /// この言語が tree-sitter ではなく手書き lexer で解析されるかを返す。
    /// `true` なら `ts_language()` は呼べない。
    #[inline]
    pub fn is_lexer_only(self) -> bool {
        matches!(self, Self::Xojo)
    }

    /// LangId を新型 DetectedLang に変換する。
    /// 既存コードは LangId を使い続け、新規コードは DetectedLang を介して
    /// backend (TreeSitter / LexerOnly) を区別できる。
    pub fn detected(self) -> DetectedLang {
        match self {
            Self::Rust => DetectedLang::TreeSitter(TreeSitterLang::Rust),
            Self::C => DetectedLang::TreeSitter(TreeSitterLang::C),
            Self::Cpp => DetectedLang::TreeSitter(TreeSitterLang::Cpp),
            Self::Python => DetectedLang::TreeSitter(TreeSitterLang::Python),
            Self::Javascript => DetectedLang::TreeSitter(TreeSitterLang::Javascript),
            Self::Typescript => DetectedLang::TreeSitter(TreeSitterLang::Typescript),
            Self::Tsx => DetectedLang::TreeSitter(TreeSitterLang::Tsx),
            Self::Go => DetectedLang::TreeSitter(TreeSitterLang::Go),
            Self::Php => DetectedLang::TreeSitter(TreeSitterLang::Php),
            Self::Java => DetectedLang::TreeSitter(TreeSitterLang::Java),
            Self::Kotlin => DetectedLang::TreeSitter(TreeSitterLang::Kotlin),
            Self::Swift => DetectedLang::TreeSitter(TreeSitterLang::Swift),
            Self::CSharp => DetectedLang::TreeSitter(TreeSitterLang::CSharp),
            Self::Bash => DetectedLang::TreeSitter(TreeSitterLang::Bash),
            Self::Ruby => DetectedLang::TreeSitter(TreeSitterLang::Ruby),
            Self::Zig => DetectedLang::TreeSitter(TreeSitterLang::Zig),
            Self::Xojo => DetectedLang::LexerOnly(LexerLang::Xojo),
        }
    }
}

/// tree-sitter バックエンドを持つ言語の列挙。
/// `ts_language()` を呼べるのはこの型だけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeSitterLang {
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
}

impl TreeSitterLang {
    /// tree-sitter Language を取得する。
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
        }
    }

    /// 表示名 (シリアライズ表現と一致)。
    pub fn display_name(self) -> &'static str {
        match self {
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
        }
    }
}

impl std::fmt::Display for TreeSitterLang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// lexer fallback でだけ解析する言語の列挙。
/// tree-sitter での parse が現実的でない言語（巨大 LR テーブルで OOM になる Xojo 等）や、
/// まだ tree-sitter grammar が無い言語をここに置く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LexerLang {
    /// Xojo: case-insensitive な GLR バックトラッキング系言語。
    /// tree-sitter-xojo は parse 中に 1GB/秒で線形にメモリ膨張するため v26.6 で削除し、
    /// 手書き lexer に置換した。
    Xojo,
}

impl LexerLang {
    /// 表示名 (シリアライズ表現と一致)。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Xojo => "xojo",
        }
    }

    /// case-insensitive な識別子を持つ言語かどうか。
    pub fn case_insensitive(self) -> bool {
        match self {
            Self::Xojo => true,
        }
    }
}

impl std::fmt::Display for LexerLang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// 検出された言語と、それを解析するためのバックエンドの組。
/// `TreeSitter` は tree-sitter Query で解析、`LexerOnly` は手書き lexer で解析する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "backend", content = "lang", rename_all = "lowercase")]
pub enum DetectedLang {
    #[serde(rename = "tree_sitter")]
    TreeSitter(TreeSitterLang),
    #[serde(rename = "lexer_only")]
    LexerOnly(LexerLang),
}

impl DetectedLang {
    /// case-insensitive な識別子を持つ言語かどうか。
    /// 現状の TreeSitter 系言語はすべて case-sensitive、LexerLang::Xojo のみ true。
    pub fn case_insensitive(self) -> bool {
        match self {
            Self::TreeSitter(_) => false,
            Self::LexerOnly(lang) => lang.case_insensitive(),
        }
    }

    /// 表示名 (例: "rust", "xojo")。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::TreeSitter(lang) => lang.display_name(),
            Self::LexerOnly(lang) => lang.display_name(),
        }
    }

    /// tree-sitter バックエンドの場合のみ Some を返す。
    /// LexerLang は None を返す（呼び出し側は分岐する責務がある）。
    pub fn tree_sitter(self) -> Option<TreeSitterLang> {
        match self {
            Self::TreeSitter(lang) => Some(lang),
            Self::LexerOnly(_) => None,
        }
    }

    /// この言語が手書き lexer で解析されるかを返す。
    #[inline]
    pub fn is_lexer_only(self) -> bool {
        matches!(self, Self::LexerOnly(_))
    }
}

impl std::fmt::Display for DetectedLang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
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

    #[test]
    fn detected_lang_rust_is_tree_sitter() {
        let d = LangId::Rust.detected();
        assert!(matches!(d, DetectedLang::TreeSitter(TreeSitterLang::Rust)));
        assert_eq!(d.display_name(), "rust");
        assert!(!d.case_insensitive());
        assert!(d.tree_sitter().is_some());
    }

    #[test]
    fn detected_lang_xojo_is_lexer_only() {
        let d = LangId::Xojo.detected();
        assert!(matches!(d, DetectedLang::LexerOnly(LexerLang::Xojo)));
        assert_eq!(d.display_name(), "xojo");
        assert!(d.case_insensitive());
        assert!(d.tree_sitter().is_none());
        assert!(d.is_lexer_only());
    }

    #[test]
    fn lang_id_xojo_is_lexer_only() {
        assert!(LangId::Xojo.is_lexer_only());
        assert!(!LangId::Rust.is_lexer_only());
    }

    #[test]
    #[should_panic(expected = "Xojo is lexer-only")]
    fn lang_id_xojo_ts_language_panics() {
        // ts_language() は LexerOnly 言語に対して panic する。
        // 呼び出し側は事前に is_lexer_only() で振り分ける義務がある。
        let _ = LangId::Xojo.ts_language();
    }

    #[test]
    fn tree_sitter_lang_provides_language() {
        // ts_language() は TreeSitterLang のみが持つ (LexerLang は持たない)。
        let lang = TreeSitterLang::Rust;
        let _ = lang.ts_language();
    }

    #[test]
    fn detected_lang_display_matches_lang_id_display() {
        // 既存の LangId Display と新型 DetectedLang display_name の互換確認。
        for lang in [
            LangId::Rust,
            LangId::Python,
            LangId::Typescript,
            LangId::Xojo,
            LangId::CSharp,
        ] {
            assert_eq!(format!("{lang}"), lang.detected().display_name());
        }
    }
}
