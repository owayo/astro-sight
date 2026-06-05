//! Xojo の lexer プロファイルと言語固有テスト。
//!
//! Xojo は case-insensitive な GLR バックトラッキング系言語で、tree-sitter-xojo の
//! `parse_file` が 1GB/秒級でメモリを線形に膨張させる事案 (実 Xojo プロジェクトで 30GB OOM)
//! を起こしたため、v26.6 で lexer-only バックエンドに移行した。

use std::collections::{HashMap, HashSet};

use super::{LexerProfile, extract_definition_from_line, strip_keyword_prefix};
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

/// Xojo の runtime entrypoint (dead-code 除外対象) の定義行 (0-indexed) を収集する。
///
/// 1. **イベントハンドラ**: `#tag Event` 〜 `#tag EndEvent` 配下の `Sub` / `Function`。
///    `#tag Events <control>` (コントロールのイベント) も `#tag Class` 直下の
///    `#tag Event` (クラスのイベント実装) も対象。Xojo ランタイムがイベント駆動で
///    呼ぶため静的 caller を持たない (引数有無は問わない)。
/// 2. **XojoUnit テストメソッド**: `Inherits TestGroup` を直接/間接に継承するクラス内の、
///    引数なしで名前が `Test` で終わる (case-insensitive) メソッド、および
///    `Setup` / `TearDown` ライフサイクルメソッド。XojoUnit が Introspection で
///    列挙・実行するため静的 caller を持たない。
pub fn runtime_entrypoint_lines(source: &[u8]) -> HashSet<usize> {
    let profile = &PROFILE;
    let Ok(src) = std::str::from_utf8(source) else {
        return HashSet::new();
    };

    let test_group_classes = collect_test_group_classes(src, profile);

    let mut lines = HashSet::new();
    let mut in_event = false;
    let mut current_class: Option<String> = None;

    for (line_num, raw_line) in src.split_inclusive('\n').enumerate() {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        let trimmed = clean.trim_start();

        // イベントブロックのタグ追跡。`#tag Event` 〜 `#tag EndEvent`
        // (`#tag EndEvents` も prefix で含む) の間を in_event とする。
        if trimmed == "#tag Event" {
            in_event = true;
            continue;
        }
        if trimmed.starts_with("#tag EndEvent") {
            in_event = false;
            continue;
        }
        // クラススコープ終了でリセット。
        if is_class_end(trimmed) {
            current_class = None;
            continue;
        }

        let Some((kind, name, col)) = extract_definition_from_line(profile, clean, line_num) else {
            continue;
        };

        match kind {
            SymbolKind::Class | SymbolKind::Module => {
                current_class = Some(name.to_ascii_lowercase());
            }
            // Sub / Function はどちらも Function kind。
            SymbolKind::Function => {
                if in_event {
                    // (1) イベントハンドラ
                    lines.insert(line_num);
                } else if let Some(cls) = &current_class
                    && test_group_classes.contains(cls)
                    && is_xojo_test_method_name(&name)
                    && is_paramless(clean, col, name.len())
                {
                    // (2) XojoUnit テストメソッド
                    lines.insert(line_num);
                }
            }
            _ => {}
        }
    }

    lines
}

/// `Inherits TestGroup` を直接/間接に継承するクラス名 (小文字化済み) の集合。
/// 間接継承 (TestGroup 派生のさらに派生) も fixed-point で解決する。
fn collect_test_group_classes(src: &str, profile: &LexerProfile) -> HashSet<String> {
    let mut inherits: HashMap<String, String> = HashMap::new();
    let mut current_class: Option<String> = None;

    for raw_line in src.split_inclusive('\n') {
        let clean = raw_line.trim_end_matches(['\r', '\n']);
        let trimmed = clean.trim_start();

        if is_class_end(trimmed) {
            current_class = None;
            continue;
        }
        // `Inherits <Base>` は extract_definition_from_line では拾えないため個別パース。
        if let Some(base) = parse_inherits(trimmed, profile.case_insensitive) {
            if let Some(cls) = &current_class {
                inherits.insert(cls.clone(), base.to_ascii_lowercase());
            }
            continue;
        }
        if let Some((kind, name, _)) = extract_definition_from_line(profile, clean, 0)
            && matches!(kind, SymbolKind::Class | SymbolKind::Module)
        {
            current_class = Some(name.to_ascii_lowercase());
        }
    }

    // base が TestGroup または既知派生クラスなら派生として取り込む (fixed-point)。
    let mut result: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for (cls, base) in &inherits {
            if result.contains(cls) {
                continue;
            }
            if base == "testgroup" || result.contains(base) {
                result.insert(cls.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// `End Class` / `End Module` / `#tag EndClass` / `#tag EndModule` を判定する。
fn is_class_end(trimmed: &str) -> bool {
    trimmed == "#tag EndClass"
        || trimmed == "#tag EndModule"
        || trimmed.eq_ignore_ascii_case("end class")
        || trimmed.eq_ignore_ascii_case("end module")
}

/// `Inherits <Base>` 行から base クラス名を取り出す。
fn parse_inherits(trimmed: &str, ci: bool) -> Option<String> {
    let after = strip_keyword_prefix(trimmed, "Inherits", ci)?;
    let name = after.trim_start();
    let end = name
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(name.len());
    if end == 0 {
        return None;
    }
    Some(name[..end].to_string())
}

/// XojoUnit のテスト/ライフサイクルメソッド名か (case-insensitive)。
/// `*Test` (Test で終わる) / `Setup` / `TearDown`。
fn is_xojo_test_method_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with("test") || lower == "setup" || lower == "teardown"
}

/// 定義行で method 名の直後が `()` (引数なし) かを判定する。
/// `name_col` は元行頭からの name 開始バイトオフセット。
fn is_paramless(line: &str, name_col: usize, name_len: usize) -> bool {
    let Some(after) = line.get(name_col + name_len..) else {
        return false;
    };
    let t = after.trim_start();
    match t.strip_prefix('(') {
        Some(rest) => rest.trim_start().starts_with(')'),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{IdentScanner, extract_symbols};
    use super::{PROFILE, runtime_entrypoint_lines};
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

    // ===== runtime entrypoint (dead-code 除外) =====

    #[test]
    fn xojo_event_handler_is_entrypoint() {
        // #15: #tag Event 配下の Function/Sub はイベントハンドラ entrypoint。
        let src = "#tag Events MyControl\n\t#tag Event\n\t\tFunction KeyDown(Key As String) As Boolean\n\t\t\tReturn False\n\t\tEnd Function\n\t#tag EndEvent\n#tag EndEvents\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(
            lines.contains(&2),
            "KeyDown (#tag Event 配下) は entrypoint: {lines:?}"
        );
    }

    #[test]
    fn xojo_testgroup_test_method_is_entrypoint() {
        // #16: Inherits TestGroup クラスの引数なし *Test は entrypoint。
        // 通常メソッド (Test で終わらない) は除外しない。
        let src = "Protected Class FooTests\nInherits TestGroup\n\tSub barTest()\n\tEnd Sub\n\tSub helperUtil()\n\tEnd Sub\nEnd Class\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(lines.contains(&2), "barTest は entrypoint: {lines:?}");
        assert!(
            !lines.contains(&4),
            "helperUtil (Test 終わりでない) は entrypoint でない: {lines:?}"
        );
    }

    #[test]
    fn xojo_non_testgroup_test_method_not_entrypoint() {
        // TestGroup を継承しないクラスの *Test は entrypoint ではない。
        let src = "Protected Class Foo\nInherits Object\n\tSub barTest()\n\tEnd Sub\nEnd Class\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(
            !lines.contains(&2),
            "非 TestGroup クラスの barTest は entrypoint でない: {lines:?}"
        );
    }

    #[test]
    fn xojo_indirect_testgroup_subclass_is_entrypoint() {
        // 同一ファイル内の間接継承 (fixed-point): TestGroup 派生のさらに派生の *Test も entrypoint。
        let src = "Protected Class Base\nInherits TestGroup\nEnd Class\nProtected Class Derived\nInherits Base\n\tSub fooTest()\n\tEnd Sub\nEnd Class\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(
            lines.contains(&5),
            "間接派生クラスの fooTest は entrypoint: {lines:?}"
        );
    }

    #[test]
    fn xojo_setup_teardown_are_entrypoints() {
        // Setup / TearDown ライフサイクルメソッドも entrypoint。
        let src = "Protected Class FooTests\nInherits TestGroup\n\tSub Setup()\n\tEnd Sub\n\tSub TearDown()\n\tEnd Sub\nEnd Class\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(lines.contains(&2), "Setup は entrypoint: {lines:?}");
        assert!(lines.contains(&4), "TearDown は entrypoint: {lines:?}");
    }

    #[test]
    fn xojo_parametrized_test_method_not_entrypoint() {
        // 引数ありの *Test は XojoUnit のテストメソッドではない (引数なしのみ)。
        let src = "Protected Class FooTests\nInherits TestGroup\n\tSub fooTest(x As Integer)\n\tEnd Sub\nEnd Class\n";
        let lines = runtime_entrypoint_lines(src.as_bytes());
        assert!(
            !lines.contains(&2),
            "引数あり fooTest は entrypoint でない: {lines:?}"
        );
    }
}
