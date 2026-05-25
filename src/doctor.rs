use crate::language::LangId;
use serde::Serialize;

const SUPPORTED_LANGUAGES: [LangId; 17] = [
    LangId::Rust,
    LangId::C,
    LangId::Cpp,
    LangId::Python,
    LangId::Javascript,
    LangId::Typescript,
    LangId::Tsx,
    LangId::Go,
    LangId::Php,
    LangId::Java,
    LangId::Kotlin,
    LangId::Swift,
    LangId::CSharp,
    LangId::Bash,
    LangId::Ruby,
    LangId::Zig,
    LangId::Xojo,
];

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub languages: Vec<LanguageStatus>,
}

#[derive(Debug, Serialize)]
pub struct LanguageStatus {
    pub language: LangId,
    pub available: bool,
    /// 言語解析バックエンド (`tree_sitter` または `lexer_only`)。
    pub backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parser_version: Option<String>,
}

/// `doctor` チェックを実行し、対応済み tree-sitter grammar を検証する。
pub fn run_doctor() -> DoctorReport {
    let statuses: Vec<LanguageStatus> = SUPPORTED_LANGUAGES
        .iter()
        .map(|&lang| {
            let backend = if lang.is_lexer_only() {
                "lexer_only"
            } else {
                "tree_sitter"
            };
            let available = check_language(lang);
            let parser_version = if available && !lang.is_lexer_only() {
                Some(lang.ts_language().abi_version().to_string())
            } else {
                None
            };
            LanguageStatus {
                language: lang,
                available,
                backend,
                parser_version,
            }
        })
        .collect();

    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        languages: statuses,
    }
}

fn check_language(lang: LangId) -> bool {
    if lang.is_lexer_only() {
        // lexer-only 言語は tree-sitter を持たないが、内蔵 lexer は常に利用可能。
        return true;
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.ts_language()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_doctor_reports_all_supported_languages() {
        let report = run_doctor();
        assert_eq!(report.languages.len(), SUPPORTED_LANGUAGES.len());
        assert!(
            report
                .languages
                .iter()
                .any(|status| status.language == LangId::Zig)
        );
    }

    #[test]
    fn available_languages_always_include_parser_version() {
        let report = run_doctor();
        for status in report.languages {
            let lexer_only = status.backend == "lexer_only";
            if status.available && !lexer_only {
                // tree-sitter 系の available 言語は parser_version を持つ。
                assert!(
                    status.parser_version.is_some(),
                    "{:?} に parser_version がありません",
                    status.language
                );
            } else {
                // unavailable または lexer-only は parser_version を持たない。
                assert!(
                    status.parser_version.is_none(),
                    "{:?} ({}) が parser_version を持っています",
                    status.language,
                    status.backend
                );
            }
        }
    }

    #[test]
    fn lexer_only_language_is_marked_lexer_only() {
        let report = run_doctor();
        let xojo = report
            .languages
            .iter()
            .find(|s| s.language == LangId::Xojo)
            .expect("xojo must be in supported list");
        assert_eq!(xojo.backend, "lexer_only");
        assert!(xojo.available, "xojo lexer is always available");
        assert!(xojo.parser_version.is_none());
    }
}
