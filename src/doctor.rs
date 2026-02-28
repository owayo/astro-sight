use crate::language::LangId;
use serde::Serialize;

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

/// Run the doctor check: verify all tree-sitter grammars are functional.
pub fn run_doctor() -> DoctorReport {
    let languages = [
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
    ];

    let statuses: Vec<LanguageStatus> = languages
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
