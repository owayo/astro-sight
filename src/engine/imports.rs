use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::import::{ImportEdge, ImportKind};

/// パース済み AST から import エッジを抽出する。
pub fn extract_imports(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<ImportEdge>> {
    let (query_src, kind) = import_query(lang_id);
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
            if *capture_name != "import.source" {
                continue;
            }

            let source_text = node.utf8_text(source).unwrap_or("").to_string();
            if source_text.is_empty() {
                continue;
            }

            // ソーステキストをクリーンアップ（文字列リテラルの引用符を除去）
            let clean_source = source_text
                .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_string();

            // コンテキスト: import 文の全テキストを取得
            // import 文ノードまで上方走査
            let import_node = find_import_statement(node);
            let context = import_node.utf8_text(source).unwrap_or("").to_string();

            // Determine the actual kind based on the pattern index
            let actual_kind = determine_kind(lang_id, m.pattern_index, kind);

            edges.push(ImportEdge {
                source: clean_source,
                line: import_node.start_position().row,
                kind: actual_kind,
                context,
            });
        }
    }

    Ok(edges)
}

/// import/use/include 文ノードまで上方走査する。
fn find_import_statement(node: Node<'_>) -> Node<'_> {
    let import_kinds = [
        "use_declaration",
        "import_statement",
        "import_from_statement",
        "import_spec",
        "import_declaration",
        "preproc_include",
        "namespace_use_declaration",
        "using_directive",
        "import_header",
        "call_expression", // require() 呼び出し用
        "call",            // Ruby の require/require_relative 用
    ];
    let mut current = Some(node);
    while let Some(n) = current {
        if import_kinds.contains(&n.kind()) {
            return n;
        }
        current = n.parent();
    }
    node
}

/// 言語とパターンに基づき実際の ImportKind を決定する。
fn determine_kind(lang_id: LangId, pattern_index: usize, default: ImportKind) -> ImportKind {
    match lang_id {
        // JS/TS: パターン 0 = Import 文、パターン 1 = require()
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            if pattern_index >= 1 {
                ImportKind::Require
            } else {
                ImportKind::Import
            }
        }
        _ => default,
    }
}

/// 言語別の import 文用 tree-sitter クエリを返す。
/// (クエリ文字列, デフォルト ImportKind) を返す。
fn import_query(lang_id: LangId) -> (&'static str, ImportKind) {
    match lang_id {
        LangId::Rust => (
            r#"(use_declaration argument: (_) @import.source)"#,
            ImportKind::Use,
        ),
        LangId::Python => (
            r#"
            (import_statement name: (dotted_name) @import.source)
            (import_from_statement module_name: (dotted_name) @import.source)
            "#,
            ImportKind::Import,
        ),
        LangId::Javascript => (
            r#"
            (import_statement source: (string) @import.source)
            (call_expression
              function: (identifier) @fn_name
              arguments: (arguments (string) @import.source)
              (#eq? @fn_name "require"))
            "#,
            ImportKind::Import,
        ),
        LangId::Typescript | LangId::Tsx => (
            r#"
            (import_statement source: (string) @import.source)
            (call_expression
              function: (identifier) @fn_name
              arguments: (arguments (string) @import.source)
              (#eq? @fn_name "require"))
            "#,
            ImportKind::Import,
        ),
        LangId::Go => (
            r#"(import_spec path: (interpreted_string_literal) @import.source)"#,
            ImportKind::Import,
        ),
        LangId::Java => (
            r#"(import_declaration (scoped_identifier) @import.source)"#,
            ImportKind::Import,
        ),
        LangId::C | LangId::Cpp => (
            r#"(preproc_include path: (_) @import.source)"#,
            ImportKind::Include,
        ),
        LangId::CSharp => (r#"(using_directive (_) @import.source)"#, ImportKind::Use),
        LangId::Php => (
            r#"
            (namespace_use_declaration
              (namespace_use_clause
                [(qualified_name) (name)] @import.source))
            "#,
            ImportKind::Use,
        ),
        LangId::Kotlin => (
            r#"(import_header (identifier) @import.source)"#,
            ImportKind::Import,
        ),
        LangId::Swift => (
            r#"(import_declaration (identifier) @import.source)"#,
            ImportKind::Import,
        ),
        LangId::Bash => ("", ImportKind::Import), // 非対応
        LangId::Zig => (
            r#"
            (builtin_function
              (builtin_identifier) @fn_name
              (arguments (string) @import.source)
              (#eq? @fn_name "@import"))
            "#,
            ImportKind::Import,
        ),
        LangId::Ruby => (
            r#"
            (call
              method: (identifier) @fn_name
              arguments: (argument_list (string) @import.source)
              (#match? @fn_name "^(require|require_relative)$"))
            "#,
            ImportKind::Require,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    /// Rust の use 宣言が正しく抽出される
    #[test]
    fn extract_imports_rust() {
        let source = b"use std::collections::HashMap;\nuse anyhow::Result;\n\nfn main() {}";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Rust).unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].source, "std::collections::HashMap");
        assert_eq!(imports[0].kind, ImportKind::Use);
        assert_eq!(imports[1].source, "anyhow::Result");
    }

    /// Python の import/from import が正しく抽出される
    #[test]
    fn extract_imports_python() {
        let source = b"import os\nfrom collections import defaultdict\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Python).unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].source, "os");
        assert_eq!(imports[0].kind, ImportKind::Import);
        assert_eq!(imports[1].source, "collections");
    }

    /// JavaScript の import と require() が正しく抽出される
    #[test]
    fn extract_imports_javascript() {
        let source = b"import { foo } from './bar';\nconst x = require('lodash');\n";
        let tree = parser::parse_source(source, LangId::Javascript).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Javascript).unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].source, "./bar");
        assert_eq!(imports[0].kind, ImportKind::Import);
        assert_eq!(imports[1].source, "lodash");
        assert_eq!(imports[1].kind, ImportKind::Require);
    }

    /// Bash は非対応で空リストを返す
    #[test]
    fn extract_imports_bash_returns_empty() {
        let source = b"#!/bin/bash\nsource ./helper.sh\n";
        let tree = parser::parse_source(source, LangId::Bash).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Bash).unwrap();
        assert!(imports.is_empty());
    }

    /// Go の import が正しく抽出される
    #[test]
    fn extract_imports_go() {
        let source = b"package main\n\nimport \"fmt\"\n\nfunc main() {}";
        let tree = parser::parse_source(source, LangId::Go).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Go).unwrap();
        assert_eq!(imports.len(), 1);
        // Go はダブルクォートを含む場合があるがクリーンアップされる
        let clean = imports[0].source.trim_matches('"');
        assert_eq!(clean, "fmt");
    }

    /// 全言語の import クエリが構文的に有効（空文字列を除く）
    #[test]
    fn import_query_all_languages_valid() {
        let all_langs = [
            LangId::Rust,
            LangId::Python,
            LangId::Javascript,
            LangId::Typescript,
            LangId::Tsx,
            LangId::Go,
            LangId::Java,
            LangId::C,
            LangId::Cpp,
            LangId::CSharp,
            LangId::Php,
            LangId::Kotlin,
            LangId::Swift,
            LangId::Ruby,
        ];

        for lang in all_langs {
            let (query_src, _kind) = import_query(lang);
            if query_src.is_empty() {
                continue;
            }
            let language = lang.ts_language();
            let result = Query::new(&language, query_src);
            assert!(
                result.is_ok(),
                "{lang:?} の import クエリがパースに失敗: {:?}",
                result.err()
            );
        }
    }

    /// determine_kind が JS/TS でパターンインデックスに応じた正しい種別を返す
    #[test]
    fn determine_kind_js_patterns() {
        assert_eq!(
            determine_kind(LangId::Javascript, 0, ImportKind::Import),
            ImportKind::Import
        );
        assert_eq!(
            determine_kind(LangId::Javascript, 1, ImportKind::Import),
            ImportKind::Require
        );
        // 非 JS/TS 言語はデフォルトを返す
        assert_eq!(
            determine_kind(LangId::Rust, 0, ImportKind::Use),
            ImportKind::Use
        );
        assert_eq!(
            determine_kind(LangId::Rust, 1, ImportKind::Use),
            ImportKind::Use
        );
    }
}
