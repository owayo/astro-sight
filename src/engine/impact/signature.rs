use crate::language::{LangId, normalize_identifier};
use crate::models::impact::{AffectedSymbol, SignatureChange};

/// case-insensitive な言語では haystack / needle を正規化した上で contains 判定する。
fn text_contains(haystack: &str, needle: &str, lang: LangId) -> bool {
    if lang.is_case_insensitive() {
        normalize_identifier(lang, haystack)
            .as_ref()
            .contains(normalize_identifier(lang, needle).as_ref())
    } else {
        haystack.contains(needle)
    }
}

/// diff 内の削除行(-)と追加行(+)から、affected シンボルの関数シグネチャ変更を検出する。
pub(crate) fn detect_signature_changes(
    diff_input: &str,
    file_path: &str,
    affected: &[AffectedSymbol],
    lang_id: LangId,
) -> Vec<SignatureChange> {
    let mut changes = Vec::new();
    let mut in_file = false;
    let mut removed_lines = Vec::new();
    let mut added_lines = Vec::new();

    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            let path = line.strip_prefix("+++ b/").unwrap_or("");
            in_file = path == file_path;
            if in_file {
                removed_lines.clear();
                added_lines.clear();
            }
        } else if line.starts_with("--- ") {
            // 次の +++ 行で処理される
        } else if in_file {
            if let Some(content) = line.strip_prefix('-') {
                removed_lines.push(content.to_string());
            } else if let Some(content) = line.strip_prefix('+') {
                added_lines.push(content.to_string());
            }
        }
    }

    for sym in affected {
        if sym.kind != "function" && sym.kind != "method" {
            continue;
        }

        let old_sig = find_signature_in_lines(&removed_lines, &sym.name, lang_id);
        let new_sig = find_signature_in_lines(&added_lines, &sym.name, lang_id);

        if let (Some(old), Some(new)) = (old_sig, new_sig)
            && old != new
        {
            changes.push(SignatureChange {
                name: sym.name.clone(),
                old_signature: old,
                new_signature: new,
            });
        }
    }

    changes
}

/// 指定された関数名を含むシグネチャ行を検索する。
pub(crate) fn find_signature_in_lines(
    lines: &[String],
    func_name: &str,
    lang_id: LangId,
) -> Option<String> {
    for line in lines {
        let trimmed = line.trim();
        if text_contains(trimmed, func_name, lang_id) && is_signature_line(trimmed) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// ヒューリスティック: 関数定義キーワードを含む行をシグネチャと判定する。
///
/// 言語固有キーワード ("fn ", "def " 等) はそのままマッチ（十分に特異的）。
/// 型キーワード ("void", "int" 等) と修飾子 ("public", "static" 等) は
/// `(` を含む行のみマッチ（関数定義は通常括弧を含むため）。
pub(crate) fn is_signature_line(line: &str) -> bool {
    // 言語固有の関数定義キーワード（十分に特異的なのでそのままマッチ）
    let lang_keywords = ["fn ", "def ", "function ", "func ", "fun "];
    if lang_keywords.iter().any(|kw| line.contains(kw)) {
        return true;
    }

    // Xojo: `Function X(...)`, `Sub X(...)`, `Event X(...)` で宣言する。
    // 言語仕様上 case-insensitive のため ASCII case-insensitive で判定する。
    let xojo_keywords = ["function ", "sub ", "event "];
    let lower_prefix: String = line
        .chars()
        .take(40)
        .flat_map(|c| c.to_lowercase())
        .collect();
    if xojo_keywords.iter().any(|kw| lower_prefix.contains(kw)) {
        return true;
    }

    // 型キーワードと修飾子は `(` を含む行のみマッチ（関数定義の確度を上げる）
    let needs_paren_keywords = [
        "void ",
        "int ",
        "string ",
        "bool ",
        "public ",
        "private ",
        "protected ",
        "static ",
        "async ",
    ];
    if line.contains('(') && needs_paren_keywords.iter().any(|kw| line.contains(kw)) {
        return true;
    }

    false
}

/// 型シンボルの定義ヘッダが変更行(+/-)に出現するか確認する。
///
/// trait/struct/class/interface/enum シンボルについて、宣言キーワードに続くシンボル名
/// （例: `trait GuestMemory`, `struct Foo`）が変更行に存在するかを検査する。
/// シンボル名が他のシンボルのシグネチャ（例: `fn read_obj(m: &impl GuestMemory)`）
/// にのみ出現する場合の偽陽性を防止する。
pub(crate) fn is_definition_header_in_changed_lines(
    diff_input: &str,
    file_path: &str,
    symbol_name: &str,
    kind: &str,
    lang_id: LangId,
) -> bool {
    let keywords: &[&str] = match kind {
        "trait" => &["trait"],
        "struct" => &["struct"],
        "class" => &["class"],
        "interface" => &["interface", "trait"],
        "enum" => &["enum"],
        _ => return true, // 非型シンボルは常にパス
    };

    let mut in_file = false;
    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if in_file
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---")))
        {
            let content = &line[1..];
            for kw in keywords {
                let pattern = format!("{kw} {symbol_name}");
                if text_contains(content, &pattern, lang_id) {
                    return true;
                }
            }
        }
    }

    false
}

/// 指定ファイルの diff 変更行(+/-)にシンボル名が出現するか確認する。
///
/// 全変更行にシンボル名が存在しない場合、変更はボディのみ
/// （例: 内部の JSX/ロジック変更）であり、呼び出し元に影響しない。
pub(crate) fn is_symbol_in_changed_lines(
    diff_input: &str,
    file_path: &str,
    symbol_name: &str,
    lang_id: LangId,
) -> bool {
    let mut in_file = false;

    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if in_file
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---")))
            && text_contains(&line[1..], symbol_name, lang_id)
        {
            return true;
        }
    }

    false
}

/// コンテキスト行（例: "    symbols::extract_symbols(...)"）から関数名の抽出を試みる。
pub(crate) fn extract_function_from_context(context: &str) -> Option<String> {
    // "fn name" パターンを検索
    if let Some(pos) = context.find("fn ") {
        let rest = &context[pos + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // 関数定義キーワードを含む行はシグネチャ行と判定される
    #[test]
    fn is_signature_line_detects_fn() {
        assert!(is_signature_line(
            "    pub fn process_data(x: i32) -> bool {"
        ));
        assert!(is_signature_line("def handle_request(self):"));
        assert!(is_signature_line("function calculate() {"));
    }

    // Xojo の Function/Sub/Event 宣言は case-insensitive に判定される
    #[test]
    fn is_signature_line_xojo_keywords() {
        assert!(is_signature_line(
            "\t\tFunction Greet(name as String = \"\") As String"
        ));
        assert!(is_signature_line("\tSub DoThing(value as Integer)"));
        assert!(is_signature_line("    Event Triggered(code as Integer)"));
        // 大文字混在でも OK
        assert!(is_signature_line(
            "  FUNCTION Compute(x as Double) as Double"
        ));
    }

    // 通常のコード行はシグネチャ行と判定されない
    #[test]
    fn is_signature_line_rejects_normal_code() {
        assert!(!is_signature_line("    let x = 42;"));
        assert!(!is_signature_line("    x += 1"));
    }

    // 修飾子のみの行（括弧なし）はシグネチャ行と判定されない
    #[test]
    fn is_signature_line_modifier_without_paren() {
        assert!(!is_signature_line("    public class Foo {"));
        assert!(!is_signature_line("    static final int X = 1;"));
        assert!(!is_signature_line("    async { task.await }"));
    }

    // 修飾子 + 括弧を含む行はシグネチャ行と判定される
    #[test]
    fn is_signature_line_modifier_with_paren() {
        assert!(is_signature_line("    public void doSomething(int x) {"));
        assert!(is_signature_line("    static int compute(float y) {"));
        assert!(is_signature_line("    async fetchData(url: string) {"));
    }

    // 型キーワード + 括弧を含む行はシグネチャ行と判定される
    #[test]
    fn is_signature_line_type_with_paren() {
        assert!(is_signature_line("    void process(int x) {"));
        assert!(is_signature_line("    int calculate(float y) {"));
        assert!(is_signature_line("    bool validate(string s) {"));
    }

    // 型キーワードのみで括弧なしの行はシグネチャ行と判定されない
    #[test]
    fn is_signature_line_type_without_paren() {
        assert!(!is_signature_line("    void* ptr = nullptr;"));
        assert!(!is_signature_line("    int count = 0;"));
        assert!(!is_signature_line("    bool flag = true;"));
    }

    // "fn name(...)" パターンから関数名を抽出できる
    #[test]
    fn extract_function_from_context_fn() {
        assert_eq!(
            extract_function_from_context("    fn process_data(x: i32) {"),
            Some("process_data".to_string())
        );
    }

    // fn キーワードが含まれない場合は None を返す
    #[test]
    fn extract_function_from_context_no_fn() {
        assert_eq!(extract_function_from_context("let x = 42;"), None);
    }

    // 変更行にシンボル名が含まれる場合 true を返す
    #[test]
    fn is_symbol_in_changed_lines_present() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-fn old_func() {}\n+fn new_func() {}";
        assert!(is_symbol_in_changed_lines(
            diff,
            "src/lib.rs",
            "old_func",
            LangId::Rust
        ));
        assert!(is_symbol_in_changed_lines(
            diff,
            "src/lib.rs",
            "new_func",
            LangId::Rust
        ));
    }

    // 変更行にシンボル名が含まれない場合 false を返す
    #[test]
    fn is_symbol_in_changed_lines_absent() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-fn old_func() {}\n+fn new_func() {}";
        assert!(!is_symbol_in_changed_lines(
            diff,
            "src/lib.rs",
            "other_func",
            LangId::Rust
        ));
    }

    // detect_signature_changes がシグネチャ変更を正しく検出する
    #[test]
    fn detect_signature_changes_detects_change() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-pub fn greet() -> String {
+pub fn greet(name: &str) -> String {
     \"hello\".to_string()
 }";
        let affected = vec![AffectedSymbol {
            name: "greet".to_string(),
            kind: "function".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "src/lib.rs", &affected, LangId::Rust);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "greet");
        assert!(changes[0].old_signature.contains("greet()"));
        assert!(changes[0].new_signature.contains("greet(name: &str)"));
    }

    // シグネチャが同一の場合は変更として報告しない
    #[test]
    fn detect_signature_changes_ignores_same_sig() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn greet() {
+fn greet() {
     // 変更はボディのみ
 }";
        let affected = vec![AffectedSymbol {
            name: "greet".to_string(),
            kind: "function".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "src/lib.rs", &affected, LangId::Rust);
        assert!(changes.is_empty());
    }

    // 非関数シンボル（struct 等）はスキップされる
    #[test]
    fn detect_signature_changes_skips_non_function() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-struct Foo { x: i32 }
+struct Foo { x: i32, y: i32 }";
        let affected = vec![AffectedSymbol {
            name: "Foo".to_string(),
            kind: "struct".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "src/lib.rs", &affected, LangId::Rust);
        assert!(changes.is_empty());
    }

    // 異なるファイルの diff は無視される
    #[test]
    fn detect_signature_changes_wrong_file() {
        let diff = "\
--- a/src/other.rs
+++ b/src/other.rs
@@ -1,3 +1,3 @@
-fn greet() {
+fn greet(x: i32) {";
        let affected = vec![AffectedSymbol {
            name: "greet".to_string(),
            kind: "function".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "src/lib.rs", &affected, LangId::Rust);
        assert!(changes.is_empty());
    }

    // is_definition_header_in_changed_lines が trait/struct の変更行を検出する
    #[test]
    fn is_definition_header_detects_trait_change() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-pub trait GuestMemory {
+pub trait GuestMemory: Send + Sync {";
        assert!(is_definition_header_in_changed_lines(
            diff,
            "src/lib.rs",
            "GuestMemory",
            "trait",
            LangId::Rust
        ));
    }

    // 型ヘッダが変更行に存在しない場合は false を返す
    #[test]
    fn is_definition_header_absent() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -5,3 +5,3 @@
-    fn read_obj(&self) -> i32;
+    fn read_obj(&self, buf: &[u8]) -> i32;";
        assert!(!is_definition_header_in_changed_lines(
            diff,
            "src/lib.rs",
            "GuestMemory",
            "trait",
            LangId::Rust
        ));
    }

    // 非型シンボルは常に true を返す
    #[test]
    fn is_definition_header_non_type_always_true() {
        assert!(is_definition_header_in_changed_lines(
            "",
            "src/lib.rs",
            "foo",
            "function",
            LangId::Rust
        ));
    }
}
