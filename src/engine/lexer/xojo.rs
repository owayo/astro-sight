//! Xojo の lexer プロファイルと言語固有テスト。
//!
//! Xojo は case-insensitive な GLR バックトラッキング系言語で、tree-sitter-xojo の
//! `parse_file` が 1GB/秒級でメモリを線形に膨張させる事案 (実 Xojo プロジェクトで 30GB OOM)
//! を起こしたため、v26.6 で lexer-only バックエンドに移行した。

use super::LexerProfile;
use crate::language::LexerLang;
use crate::models::symbol::SymbolKind;

/// Xojo の定義 keyword 一覧。`End Sub` などの closing 行は除外する。
static XOJO_DEFINITION_KEYWORDS: &[(&str, SymbolKind)] = &[
    ("Class", SymbolKind::Class),
    ("Module", SymbolKind::Module),
    ("Window", SymbolKind::Class),
    ("Interface", SymbolKind::Interface),
    ("Structure", SymbolKind::Struct),
    ("Sub", SymbolKind::Function),
    ("Function", SymbolKind::Function),
    ("Property", SymbolKind::Field),
    ("Const", SymbolKind::Constant),
    ("Enum", SymbolKind::Enum),
    ("Event", SymbolKind::Method),
    ("Constructor", SymbolKind::Method),
    ("Destructor", SymbolKind::Method),
];

static XOJO_MODIFIER_KEYWORDS: &[&str] = &[
    "Public",
    "Protected",
    "Private",
    "Global",
    "Shared",
    "Static",
];

static XOJO_LINE_COMMENT_STARTS: &[&str] = &["'", "//", "REM "];

static XOJO_STRING_DELIMITERS: &[char] = &['"'];

static XOJO_END_PREFIX_KEYWORDS: &[&str] = &["End"];

pub static PROFILE: LexerProfile = LexerProfile {
    lang: LexerLang::Xojo,
    case_insensitive: true,
    line_comment_starts: XOJO_LINE_COMMENT_STARTS,
    block_comments: &[],
    string_delimiters: XOJO_STRING_DELIMITERS,
    modifier_keywords: XOJO_MODIFIER_KEYWORDS,
    definition_keywords: XOJO_DEFINITION_KEYWORDS,
    end_prefix_keywords: XOJO_END_PREFIX_KEYWORDS,
};

#[cfg(test)]
mod tests {
    use super::super::{IdentScanner, extract_symbols};
    use super::PROFILE;
    use crate::language::LexerLang;

    const XOJO_SAMPLE: &str = r#"' This is a sample Xojo class
Class Greeter
  Const DefaultName as String = "World"

  Property defaultName as String

  Sub Greet(name as String)
    System.Print("Hello, " + name)
  End Sub

  Function Counter() as Integer
    Return 42
  End Function
End Class

Module Helpers
  Sub LogIt(msg as String)
    System.Print(msg)
  End Sub
End Module
"#;

    #[test]
    fn xojo_extracts_class_module_and_methods() {
        let symbols = extract_symbols(XOJO_SAMPLE.as_bytes(), LexerLang::Xojo);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "Greeter",
            "DefaultName",
            "defaultName",
            "Greet",
            "Counter",
            "Helpers",
            "LogIt",
        ] {
            assert!(
                names.contains(&expected),
                "expected `{expected}` in {names:?}"
            );
        }
    }

    #[test]
    fn xojo_does_not_extract_end_lines() {
        let symbols = extract_symbols(XOJO_SAMPLE.as_bytes(), LexerLang::Xojo);
        for sym in &symbols {
            assert_ne!(
                sym.name, "Sub",
                "`End Sub` の `Sub` を definition として誤検出している"
            );
            assert_ne!(sym.name, "Class");
        }
    }

    #[test]
    fn xojo_assigns_container_to_methods() {
        let symbols = extract_symbols(XOJO_SAMPLE.as_bytes(), LexerLang::Xojo);
        let greet = symbols.iter().find(|s| s.name == "Greet").expect("Greet");
        assert_eq!(greet.container.as_deref(), Some("Greeter"));
        let log_it = symbols.iter().find(|s| s.name == "LogIt").expect("LogIt");
        assert_eq!(log_it.container.as_deref(), Some("Helpers"));
    }

    #[test]
    fn xojo_ident_scanner_skips_comments_and_strings() {
        let src = r#"' Class Foo
Sub Greet()
  Dim s as String = "Class Bar"
  ' Sub Baz()
End Sub
"#;
        let tokens: Vec<&str> = IdentScanner::new(src.as_bytes(), &PROFILE)
            .map(|t| t.text)
            .collect();
        assert!(
            !tokens.contains(&"Foo"),
            "comment 内 Foo を拾ってはいけない"
        );
        assert!(!tokens.contains(&"Bar"), "string 内 Bar を拾ってはいけない");
        assert!(
            !tokens.contains(&"Baz"),
            "comment 内 Baz を拾ってはいけない"
        );
        assert!(tokens.contains(&"Greet"));
        assert!(tokens.contains(&"s"));
    }

    #[test]
    fn xojo_case_insensitive_identifier_match() {
        // case_insensitive で `greet` と `GREET` を同一視できる。
        let src = b"Sub Greet()\n  Call MYFUNC()\nEnd Sub\n";
        let tokens: Vec<String> = IdentScanner::new(src, &PROFILE)
            .map(|t| t.text.to_ascii_lowercase())
            .collect();
        assert!(tokens.contains(&"greet".to_string()));
        assert!(tokens.contains(&"myfunc".to_string()));
    }

    #[test]
    fn xojo_block_comment_continuation_skips_definition() {
        // Xojo 自体には /* */ ブロックコメントは無いが、profile.block_comments が空でも
        // 通常の line comment 経路で `Sub` 誤検出を抑制できることを確認。
        let src = "' Sub Hidden()\nSub Visible()\nEnd Sub\n";
        let symbols = extract_symbols(src.as_bytes(), LexerLang::Xojo);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Visible"));
        assert!(!names.contains(&"Hidden"));
    }
}
