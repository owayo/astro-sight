use crate::language::LangId;
use serde::Serialize;

const SUPPORTED_LANGUAGES: [LangId; 16] = [
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parser_version: Option<String>,
}

/// `doctor` チェックを実行し、対応済み tree-sitter grammar を検証する。
pub fn run_doctor() -> DoctorReport {
    let statuses: Vec<LanguageStatus> = SUPPORTED_LANGUAGES
        .iter()
        .map(|&lang| {
            let available = check_language(lang);
            LanguageStatus {
                language: lang,
                available,
                parser_version: if available {
                    Some(lang.ts_language().abi_version().to_string())
                } else {
                    None
                },
            }
        })
        .collect();

    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        languages: statuses,
    }
}

fn check_language(lang: LangId) -> bool {
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
            if status.available {
                assert!(
                    status.parser_version.is_some(),
                    "{:?} に parser_version がありません",
                    status.language
                );
            } else {
                assert!(
                    status.parser_version.is_none(),
                    "{:?} が unavailable なのに parser_version を持っています",
                    status.language
                );
            }
        }
    }
}
