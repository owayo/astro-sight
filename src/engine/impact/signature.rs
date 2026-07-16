use crate::engine::diff::{HunkBodyLine, HunkProgress, parse_hunk_header};
use crate::language::{LangId, normalize_identifier};
use crate::models::impact::{AffectedSymbol, SignatureChange};

/// diff 内の削除行(-)と追加行(+)から、affected シンボルの関数シグネチャ変更を検出する。
pub(crate) fn detect_signature_changes(
    diff_input: &str,
    file_path: &str,
    affected: &[AffectedSymbol],
    lang_id: LangId,
) -> Vec<SignatureChange> {
    let mut changes = Vec::new();
    let mut in_file = false;
    let mut active_hunk: Option<HunkProgress> = None;
    let mut removed_lines = Vec::new();
    let mut added_lines = Vec::new();

    for line in diff_input.lines() {
        // hunk 本体を消費中はヘッダに見える行も本体行として扱う
        // (削除/追加行コンテンツが `--- a/...` / `+++ b/...` の形でも誤認しない)。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_file {
                match consumed {
                    HunkBodyLine::Removed(c) => removed_lines.push(c.to_string()),
                    HunkBodyLine::Added(c) => added_lines.push(c.to_string()),
                    HunkBodyLine::Context | HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("+++ b/") {
            let path = line.strip_prefix("+++ b/").unwrap_or("");
            in_file = path == file_path;
            if in_file {
                removed_lines.clear();
                added_lines.clear();
            }
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            active_hunk = Some(HunkProgress::new(&hunk));
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
    let func_name = bare_symbol_name(func_name);
    for line in lines {
        let trimmed = line.trim();
        if line_has_identifier(trimmed, func_name, lang_id) && is_signature_line(trimmed) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// `Type::method` 形式の名前でも、シグネチャ行では末尾の識別子だけを照合する。
fn bare_symbol_name(symbol_name: &str) -> &str {
    symbol_name
        .rsplit(|c: char| !(c == '_' || c.is_alphanumeric()))
        .find(|part| !part.is_empty())
        .unwrap_or(symbol_name)
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
    // 言語仕様上 case-insensitive のため、行頭から 40 バイト範囲を ASCII case-insensitive で
    // 走査する (ホットパスのため中間 String を確保しない)。
    // Xojo の identifier は ASCII のみなので bytes 単位の比較で十分。
    let bytes = line.as_bytes();
    let head = &bytes[..bytes.len().min(40)];
    let xojo_keywords: [&[u8]; 3] = [b"function ", b"sub ", b"event "];
    if xojo_keywords
        .iter()
        .any(|kw| head.windows(kw.len()).any(|w| w.eq_ignore_ascii_case(kw)))
    {
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
    let mut active_hunk: Option<HunkProgress> = None;

    for line in diff_input.lines() {
        // hunk 本体を消費中はヘッダに見える行も本体行として扱う (FN 防止)。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_file {
                let content = match consumed {
                    HunkBodyLine::Added(c) | HunkBodyLine::Removed(c) => Some(c),
                    HunkBodyLine::Context | HunkBodyLine::Metadata => None,
                };
                if let Some(content) = content
                    && line_has_identifier(content, symbol_name, lang_id)
                {
                    return true;
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("+++ b/") {
            in_file = line.strip_prefix("+++ b/").unwrap_or("") == file_path;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            active_hunk = Some(HunkProgress::new(&hunk));
        }
    }

    false
}

/// 行を識別子境界 (非英数+アンダースコア以外) で分割し、`symbol` と一致する
/// トークンが含まれるか判定する。
///
/// `text_contains` 系の substring 判定だと汎用名 (`e` / `row` / `setting` 等) が
/// 長い識別子 (`PhoneEvent`, `arrow`, `mySetting` 等) の部分一致で常に true となり、
/// cross-file 影響分析の対象が爆発して impact Pass2 のメモリが線形に膨らんでいた。
/// 識別子境界で分割してから比較することで false-positive を排除する。
fn line_has_identifier(line: &str, symbol: &str, lang: LangId) -> bool {
    let target = normalize_identifier(lang, symbol);
    line.split(|c: char| !(c == '_' || c.is_alphanumeric()))
        .filter(|s| !s.is_empty())
        .any(|tok| normalize_identifier(lang, tok).as_ref() == target.as_ref())
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

/// old/new signature のトップレベル引数の個数が同じかを返す。
/// どちらかの個数を数えられない (括弧が見つからない等) 場合は保守的に false
/// (= arity が変わった扱いで blocking 維持)。
pub(crate) fn same_top_level_arity(old_sig: &str, new_sig: &str) -> bool {
    match (
        top_level_param_count(old_sig),
        top_level_param_count(new_sig),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// signature 文字列の最初の `(...)` 内トップレベル引数個数を数える。
/// generics / ネスト括弧のカンマは深さ追跡で除外する (`fn f(a: HashMap<K, V>)` は 1)。
fn top_level_param_count(sig: &str) -> Option<usize> {
    let open = sig.find('(')?;
    let mut depth = 0usize;
    let mut count = 0usize;
    let mut has_any = false;
    for ch in sig[open..].chars() {
        match ch {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | '>' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => count += 1,
            c if !c.is_whitespace() && depth >= 1 => has_any = true,
            _ => {}
        }
    }
    Some(if has_any { count + 1 } else { 0 })
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

    /// detect_signature_changes は hunk 本体先頭の `+++ b/x` (追加行コンテンツ) で
    /// in_file を誤切替せず、後続の signature 変更を取りこぼさない (FN 回帰防止)。
    #[test]
    fn detect_signature_changes_body_line_looks_like_header() {
        // hunk old=1,new=2。本体先頭に `+++ b/x`、その後に foo の signature 変更。
        let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,1 +1,2 @@\n+++ b/x\n-fn foo(a: i32) {}\n+fn foo(a: i32, b: i32) {}\n";
        let affected = vec![AffectedSymbol {
            name: "foo".to_string(),
            kind: "function".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "lib.rs", &affected, LangId::Rust);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "foo");
    }

    /// is_symbol_in_changed_lines も hunk 本体の `+++ b/x` を追加行として扱い、
    /// in_file を誤切替せず、本体先頭以降の参照を取りこぼさない。
    #[test]
    fn is_symbol_in_changed_lines_body_line_looks_like_header() {
        let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,1 +1,2 @@\n+++ b/x\n-let v = bar;\n+let v = bar();\n";
        assert!(is_symbol_in_changed_lines(
            diff,
            "lib.rs",
            "bar",
            LangId::Rust
        ));
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

    // Xojo キーワードを後段 (40 バイト位置以降) に含む通常コード行は誤検出しない
    // (バイト先頭 40 バイトに ASCII 大小無視で完全一致するキーワードがある場合のみ true)
    #[test]
    fn is_signature_line_xojo_keyword_not_at_head_is_rejected() {
        // 41 文字目以降にキーワードが現れる行は signature ではない
        let leading = " ".repeat(41);
        let line = format!("{leading}Function NotASig() As Integer");
        assert!(!is_signature_line(&line));
        // 行頭が `i` で始まる通常代入は誤検出しない (Xojo `i` ではない)
        assert!(!is_signature_line("    items = function_call()"));
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

    // 対象名を prefix に持つ別関数の変更はシグネチャ変更として扱わない
    #[test]
    fn detect_signature_changes_ignores_prefix_function_names() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn detect_api_changes_skips_module_declaration() {
+fn detect_api_changes_modified_includes_multiline_signature_change() {
 }";
        let affected = vec![AffectedSymbol {
            name: "detect_api_changes".to_string(),
            kind: "function".to_string(),
            change_type: "modified".to_string(),
        }];
        let changes = detect_signature_changes(diff, "src/lib.rs", &affected, LangId::Rust);
        assert!(
            changes.is_empty(),
            "prefix が一致するだけの別関数を detect_api_changes のシグネチャ変更として扱わない"
        );
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

    /// arity 比較: generics / ネスト括弧内のカンマはトップレベル引数として数えない。
    #[test]
    fn same_top_level_arity_ignores_nested_commas() {
        // 型のみ変更 (arity 1 のまま)
        assert!(same_top_level_arity(
            "pub fn my_system(r: Res<u32>)",
            "pub fn my_system(r: Option<Res<u32>>)"
        ));
        // generics 内カンマは数えない
        assert!(same_top_level_arity(
            "fn f(a: HashMap<K, V>)",
            "fn f(a: BTreeMap<K, V>)"
        ));
        // 引数追加 (arity 1 → 2) は不一致
        assert!(!same_top_level_arity(
            "fn f(a: u32)",
            "fn f(a: u32, b: bool)"
        ));
        // 引数なし ↔ あり
        assert!(!same_top_level_arity("fn f()", "fn f(a: u32)"));
        assert!(same_top_level_arity("fn f()", "fn f()"));
        // ネスト fn 型 / 戻り値の `->` に惑わされない
        assert!(same_top_level_arity(
            "fn f(cb: fn(u32) -> bool) -> Vec<u8>",
            "fn f(cb: fn(i64) -> bool) -> Vec<u8>"
        ));
        // 括弧が無い signature は保守的に不一致 (blocking 維持)
        assert!(!same_top_level_arity("const X: u32", "const X: u64"));
    }
}
