use anyhow::Result;
use std::collections::HashSet;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::call::{CallEdge, CallEndpoint, CallSite};
use crate::models::location::Range;

/// パース済み AST からコールエッジを抽出する。
/// `filter_function` 指定時は caller が一致するもののみ返す。
pub fn extract_calls(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    filter_function: Option<&str>,
) -> Result<Vec<CallEdge>> {
    let query_src = call_query(lang_id);
    if query_src.is_empty() {
        return Ok(Vec::new());
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut edges = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];
            if !capture_name.ends_with("callee") {
                continue;
            }

            let callee_name = node.utf8_text(source).unwrap_or("").to_string();
            if callee_name.is_empty() {
                continue;
            }

            // Find the enclosing function (caller)
            let caller = find_enclosing_function(node, source, lang_id);
            let caller = match caller {
                Some(c) => c,
                None => continue, // top-level call, skip
            };

            if let Some(filter) = filter_function
                && caller.name != filter
            {
                continue;
            }

            // call_expression node is the parent of the callee identifier
            let call_node = node.parent().unwrap_or(node);
            let callee_range = Range::from(call_node.range());

            edges.push(CallEdge {
                caller,
                callee: CallEndpoint {
                    name: callee_name,
                    range: callee_range,
                },
                call_site: CallSite {
                    line: node.start_position().row,
                    column: node.start_position().column,
                },
            });
        }
    }

    Ok(edges)
}

/// ファイル全体の呼び出し先名 (callee) の集合を抽出する。
/// `extract_calls` はトップレベル呼び出し (caller が関数本体に属さないもの) を除外するため、
/// CLI スクリプトの `main()` 呼び出しや bash エントリポイントのコマンド呼び出しを
/// 拾えない。本関数は caller の有無を問わず純粋に callee 名を収集する。
pub fn extract_all_callees(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
) -> Result<HashSet<String>> {
    let query_src = call_query(lang_id);
    if query_src.is_empty() {
        return Ok(HashSet::new());
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut callees = HashSet::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            if !capture_name.ends_with("callee") {
                continue;
            }
            let node = capture.node;
            let callee_name = node.utf8_text(source).unwrap_or("").to_string();
            if !callee_name.is_empty() {
                callees.insert(callee_name);
            }
        }
    }

    Ok(callees)
}

/// AST を上方走査し、最も近い関数定義を見つける。
fn find_enclosing_function(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<CallEndpoint> {
    let func_kinds = function_node_kinds(lang_id);
    let mut current = node.parent();

    while let Some(n) = current {
        if func_kinds.contains(&n.kind()) {
            let name = find_function_name(n, source, lang_id)?;
            return Some(CallEndpoint {
                name,
                range: Range::from(n.range()),
            });
        }
        current = n.parent();
    }

    None
}

/// 言語ごとの関数ノード種別を返す。
fn function_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &["function_item"],
        LangId::C | LangId::Cpp => &["function_definition"],
        LangId::Python => &["function_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
        ],
        LangId::Go => &["function_declaration", "method_declaration"],
        LangId::Php => &["function_definition", "method_declaration"],
        LangId::Java => &["method_declaration", "constructor_declaration"],
        LangId::Kotlin => &["function_declaration"],
        LangId::Swift => &["function_declaration"],
        LangId::CSharp => &["method_declaration", "constructor_declaration"],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &["method", "singleton_method"],
        LangId::Zig => &["function_declaration", "test_declaration"],
        LangId::Xojo => &[
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
        ],
    }
}

/// 関数ノードの名前を抽出する。
fn find_function_name(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<String> {
    // まず "name" フィールドを試行（多くの言語で共通）
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok().map(|s| s.to_string());
    }

    // Kotlin: function_declaration > simple_identifier
    if lang_id == LangId::Kotlin {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "simple_identifier" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
        }
    }

    // C/Rust の function_definition: declarator > function_declarator > declarator (identifier)
    if let Some(decl) = node.child_by_field_name("declarator") {
        if let Some(inner) = decl.child_by_field_name("declarator") {
            return inner.utf8_text(source).ok().map(|s| s.to_string());
        }
        return decl.utf8_text(source).ok().map(|s| s.to_string());
    }

    // フォールバック: 最初の identifier 子ノード
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" || k == "field_identifier" || k == "word" {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }

    None
}

/// 言語別の call expression 用 tree-sitter クエリを返す。
fn call_query(lang_id: LangId) -> &'static str {
    match lang_id {
        LangId::Rust => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (scoped_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::C => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            "#
        }
        LangId::Cpp => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (qualified_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::Python => {
            r#"
            (call function: (identifier) @direct.callee)
            (call function: (attribute attribute: (identifier) @method.callee))
            "#
        }
        LangId::Javascript => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Typescript | LangId::Tsx => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Go => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (selector_expression field: (field_identifier) @method.callee))
            "#
        }
        LangId::Php => {
            r#"
            (function_call_expression function: (name) @direct.callee)
            (member_call_expression name: (name) @method.callee)
            (scoped_call_expression name: (name) @scoped.callee)
            "#
        }
        LangId::Java => {
            r#"
            (method_invocation name: (identifier) @direct.callee)
            (method_invocation object: (identifier) name: (identifier) @method.callee)
            "#
        }
        LangId::Kotlin => {
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression (navigation_expression (simple_identifier) @method.callee))
            "#
        }
        LangId::Swift => {
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression
              (navigation_expression
                (navigation_suffix suffix: (simple_identifier) @method.callee)))
            "#
        }
        LangId::CSharp => {
            r#"
            (invocation_expression function: (identifier) @direct.callee)
            (invocation_expression function: (member_access_expression name: (identifier) @method.callee))
            "#
        }
        LangId::Bash => {
            r#"
            (command name: (command_name (word) @direct.callee))
            "#
        }
        LangId::Ruby => {
            r#"
            (call method: (identifier) @direct.callee)
            "#
        }
        LangId::Zig => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression member: (identifier) @method.callee))
            "#
        }
        LangId::Xojo => {
            r#"
            (array_or_call_expression function: (identifier) @direct.callee)
            (array_or_call_expression function: (member_expression property: (identifier) @method.callee))
            "#
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    /// Rust の関数呼び出しを正しく抽出する
    #[test]
    fn extract_calls_rust() {
        let source = b"fn main() { helper(); }\nfn helper() {}";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, None).unwrap();
        assert!(!edges.is_empty());
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
        assert!(edges.iter().any(|e| e.caller.name == "main"));
    }

    /// filter_function で特定の caller のみ抽出できる
    #[test]
    fn extract_calls_with_filter() {
        let source = b"fn a() { b(); }\nfn b() { c(); }\nfn c() {}";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, Some("a")).unwrap();
        assert!(edges.iter().all(|e| e.caller.name == "a"));
    }

    /// Python の関数呼び出しを抽出する
    #[test]
    fn extract_calls_python() {
        let source = b"def main():\n    helper()\ndef helper():\n    pass\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Python, None).unwrap();
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
    }

    /// JavaScript の関数呼び出しを抽出する
    #[test]
    fn extract_calls_javascript() {
        let source = b"function main() { helper(); }\nfunction helper() {}";
        let tree = parser::parse_source(source, LangId::Javascript).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Javascript, None).unwrap();
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
    }

    /// 空クエリの言語でも空配列を返しパニックしない
    #[test]
    fn extract_calls_empty_source() {
        let source = b"";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, None).unwrap();
        assert!(edges.is_empty());
    }

    /// 全言語の call_query が空文字でないことを確認（Bash 含む）
    #[test]
    fn call_query_all_languages_non_empty() {
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
            LangId::Ruby,
            LangId::Zig,
            LangId::Xojo,
        ];
        for lang in &languages {
            let q = call_query(*lang);
            assert!(!q.is_empty(), "{:?} should have a call query", lang);
        }
    }

    /// function_node_kinds が全言語で空でないことを確認
    #[test]
    fn function_node_kinds_all_languages() {
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
            LangId::Ruby,
            LangId::Zig,
            LangId::Xojo,
        ];
        for lang in &languages {
            let kinds = function_node_kinds(*lang);
            assert!(
                !kinds.is_empty(),
                "{:?} should have function node kinds",
                lang
            );
        }
    }
}
