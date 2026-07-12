use std::collections::HashSet;

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
            let mut clean_source = source_text
                .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_string();

            // PHP grouped use: `use App\Services\{Mailer, Logger};` の clause は宣言側の
            // namespace prefix を含まないため、単独 use (`App\Services\Mailer`) と同じ
            // 完全修飾形式に合成する。
            if lang_id == LangId::Php
                && let Some(prefix) = php_group_use_prefix(node, source)
            {
                clean_source = format!("{prefix}\\{clean_source}");
            }

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

/// ファイル内の import/use 文が占める行 (0-indexed) の集合を返す。
///
/// 複数行 grouped use ブロック (`use foo::{\n  a,\n  b,\n};`) の継続行も含むため、
/// 行頭テキスト判定では拾えない継続行のシンボル参照を import として識別できる。
/// `is_modified_closed_in_diff` の closed 判定で「import 行の参照は signature 変更に
/// 追随不要」と扱うために使う (api.mod 誤検出 2026-05-31 対応)。
pub fn import_statement_lines(root: Node<'_>) -> HashSet<usize> {
    let mut lines = HashSet::new();
    collect_import_statement_lines(root, &mut lines);
    lines
}

/// import 系ノードの行範囲を再帰的に集める。import 文の中はさらに潜らない。
fn collect_import_statement_lines(node: Node<'_>, lines: &mut HashSet<usize>) {
    // 宣言系のみ対象 (require()/@import 等の呼び出し式は実行コードなので除く)。
    const IMPORT_STATEMENT_KINDS: &[&str] = &[
        "use_declaration",
        "import_statement",
        "import_from_statement",
        "import_declaration",
        "namespace_use_declaration",
        "using_directive",
        "import_header",
        "preproc_include",
    ];
    if IMPORT_STATEMENT_KINDS.contains(&node.kind()) {
        for row in node.start_position().row..=node.end_position().row {
            lines.insert(row);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_import_statement_lines(child, lines);
    }
}

/// PHP grouped use の clause 内 capture に対し、宣言側の namespace prefix を返す。
/// 親 chain に `namespace_use_group` が無い (単独 use) 場合は `None`。
fn php_group_use_prefix(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut in_group = false;
    let mut cur = node.parent()?;
    loop {
        match cur.kind() {
            "namespace_use_group" => in_group = true,
            "namespace_use_declaration" => break,
            _ => {}
        }
        cur = cur.parent()?;
    }
    if !in_group {
        return None;
    }
    let mut cursor = cur.walk();
    for child in cur.named_children(&mut cursor) {
        if child.kind() == "namespace_name" {
            return child.utf8_text(source).ok().map(str::to_string);
        }
    }
    None
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
            (import_statement name: (aliased_import name: (dotted_name) @import.source))
            (import_from_statement module_name: (dotted_name) @import.source)
            (import_from_statement module_name: (relative_import) @import.source)
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
            (namespace_use_declaration
              (namespace_use_group
                (namespace_use_clause
                  [(qualified_name) (name)] @import.source)))
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
        LangId::Bash => (
            // bash の `source <path>` / `. <path>` によるファイル読み込みを import として扱う。
            // 最初の word/string 引数を source として抽出する (アンカー `.` で command_name
            // 直後のもののみ)。
            r#"
            (command
              name: (command_name (word) @_cmd) .
              argument: [(word) (string) (raw_string)] @import.source
              (#match? @_cmd "^(source|\\.)$"))
            "#,
            ImportKind::Import,
        ),
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
        // Xojo の using_statement は grammar rule 上存在するが node-types.json に
        // 公開されないため、現時点では import 抽出は非対応 (後続課題)。
        LangId::Xojo => ("", ImportKind::Use),
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

    /// Bash の `source <file>` と `. <file>` をファイル読み込み import として抽出する
    #[test]
    fn extract_imports_bash_source_and_dot() {
        let source = b"#!/bin/bash\n\
source ./helper.sh\n\
. ./utils.sh\n\
source \"./quoted.sh\"\n\
echo not_source\n";
        let tree = parser::parse_source(source, LangId::Bash).unwrap();
        let root = tree.root_node();

        let imports = extract_imports(root, source, LangId::Bash).unwrap();
        let sources: Vec<String> = imports
            .iter()
            .map(|i| i.source.trim_matches('"').to_string())
            .collect();
        assert!(
            sources.contains(&"./helper.sh".to_string()),
            "source <file> を import として拾うべき: {sources:?}"
        );
        assert!(
            sources.contains(&"./utils.sh".to_string()),
            ". <file> (source の短縮形) を import として拾うべき: {sources:?}"
        );
        assert!(
            sources.iter().any(|s| s.contains("quoted.sh")),
            "クォート付きパスも拾うべき: {sources:?}"
        );
        // 通常のコマンドは import に含めない
        assert!(
            !sources.iter().any(|s| s == "not_source"),
            "通常コマンドの引数は import に入ってはならない: {sources:?}"
        );
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
            LangId::Zig,
            LangId::Xojo,
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

    /// Python の aliased import / relative import を取りこぼさない
    /// (Issue 2026-07-10-imports-python-alias-relative-php-group)。
    #[test]
    fn python_aliased_and_relative_imports_extracted() {
        let source = b"import numpy as np\nimport a.b.c as abc\nfrom . import sibling\nfrom .relative import helper\nfrom ..pkg import thing\n";
        let tree = crate::engine::parser::parse_source(source, LangId::Python).unwrap();
        let edges = extract_imports(tree.root_node(), source, LangId::Python).unwrap();
        let sources: Vec<&str> = edges.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, vec!["numpy", "a.b.c", ".", ".relative", "..pkg"]);
    }

    /// PHP grouped use は宣言側 prefix と合成して完全修飾で出力する。
    #[test]
    fn php_grouped_use_extracted_with_prefix() {
        let source = b"<?php\nuse App\\Services\\{Mailer, Logger};\nuse Single\\Plain;\n";
        let tree = crate::engine::parser::parse_source(source, LangId::Php).unwrap();
        let edges = extract_imports(tree.root_node(), source, LangId::Php).unwrap();
        let sources: Vec<&str> = edges.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains(&"App\\Services\\Mailer"), "{sources:?}");
        assert!(sources.contains(&"App\\Services\\Logger"), "{sources:?}");
        assert!(sources.contains(&"Single\\Plain"), "{sources:?}");
    }
}
