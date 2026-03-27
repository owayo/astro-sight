use crate::models::impact::{AffectedSymbol, SignatureChange};

/// diff 内の削除行(-)と追加行(+)から、affected シンボルの関数シグネチャ変更を検出する。
pub(crate) fn detect_signature_changes(
    diff_input: &str,
    file_path: &str,
    affected: &[AffectedSymbol],
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

        let old_sig = find_signature_in_lines(&removed_lines, &sym.name);
        let new_sig = find_signature_in_lines(&added_lines, &sym.name);

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
pub(crate) fn find_signature_in_lines(lines: &[String], func_name: &str) -> Option<String> {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.contains(func_name) && is_signature_line(trimmed) {
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
                if content.contains(&pattern) {
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
) -> bool {
    let mut in_file = false;

    for line in diff_input.lines() {
        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if in_file
            && ((line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---")))
            && line[1..].contains(symbol_name)
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
        assert!(is_symbol_in_changed_lines(diff, "src/lib.rs", "old_func"));
        assert!(is_symbol_in_changed_lines(diff, "src/lib.rs", "new_func"));
    }

    // 変更行にシンボル名が含まれない場合 false を返す
    #[test]
    fn is_symbol_in_changed_lines_absent() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-fn old_func() {}\n+fn new_func() {}";
        assert!(!is_symbol_in_changed_lines(
            diff,
            "src/lib.rs",
            "other_func"
        ));
    }
}
