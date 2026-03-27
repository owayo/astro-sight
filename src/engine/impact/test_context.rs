use std::collections::HashMap;

use camino::Utf8Path;

use crate::engine::parser;
use crate::language::LangId;

use super::{ParsedFile, descendant_for_range};

/// シンボルがテストコンテキスト内にあるか確認する。
///
/// テストシンボルは cross-file 影響を伝播すべきでない：
/// - テスト関数はプロダクションコードから呼ばれない
/// - テストヘルパーの変更はテストモジュールのみに影響する
///
/// 2層のアプローチを使用する：
/// 1. ファイルパスベースの判定（高速、全言語対応）
/// 2. AST ベースの判定（精密、言語固有）
pub(crate) fn is_in_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
    lang_id: LangId,
    path: &str,
) -> bool {
    // ファイルパスベースの判定（全言語共通）
    if is_test_file_path(path) {
        return true;
    }
    // AST ベースの判定（言語別）
    match lang_id {
        LangId::Rust => is_rust_test_context(root, source, symbol_range),
        LangId::Python => is_python_test_context(root, source, symbol_range),
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            is_js_test_context(root, source, symbol_range)
        }
        LangId::Java | LangId::Kotlin => is_jvm_test_context(root, source, symbol_range),
        LangId::CSharp => is_csharp_test_context(root, source, symbol_range),
        LangId::Ruby => is_ruby_test_context(root, source, symbol_range),
        _ => false,
    }
}

/// ファイルパスからテストファイルかどうかを判定する。
pub(crate) fn is_test_file_path(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let filename_lower = filename.to_lowercase();

    // Go: *_test.go
    if filename_lower.ends_with("_test.go") {
        return true;
    }
    // JS/TS: *.test.ts, *.spec.ts, *.test.js, *.spec.js, *.test.tsx, *.spec.tsx
    if filename_lower.contains(".test.") || filename_lower.contains(".spec.") {
        return true;
    }
    // Python: test_*.py, *_test.py, conftest.py
    if filename_lower.ends_with(".py")
        && (filename_lower.starts_with("test_")
            || filename_lower.ends_with("_test.py")
            || filename_lower == "conftest.py")
    {
        return true;
    }
    // Java/Kotlin: *Test.java, *Tests.java, *Test.kt, *Tests.kt
    if filename.ends_with("Test.java")
        || filename.ends_with("Tests.java")
        || filename.ends_with("Test.kt")
        || filename.ends_with("Tests.kt")
    {
        return true;
    }
    // C#: *Test.cs, *Tests.cs
    if filename.ends_with("Test.cs") || filename.ends_with("Tests.cs") {
        return true;
    }
    // Ruby: *_test.rb, *_spec.rb
    if filename_lower.ends_with("_test.rb") || filename_lower.ends_with("_spec.rb") {
        return true;
    }
    // PHP: *Test.php, *Tests.php
    if filename.ends_with("Test.php") || filename.ends_with("Tests.php") {
        return true;
    }
    // C/Cpp: *_test.c, *_test.cpp, *_test.cc
    if filename_lower.ends_with("_test.c")
        || filename_lower.ends_with("_test.cpp")
        || filename_lower.ends_with("_test.cc")
    {
        return true;
    }
    // Bash: *.bats, *_test.sh
    if filename_lower.ends_with(".bats") || filename_lower.ends_with("_test.sh") {
        return true;
    }
    false
}

/// Rust: `#[test]` attribute / `#[cfg(test)]` module
fn is_rust_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "function_item" && has_attribute_text(n, source, "test") {
            return true;
        }
        if n.kind() == "mod_item" && has_attribute_text(n, source, "cfg(test)") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Python: `test_*` 関数名 / `TestCase` サブクラス内
fn is_python_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "function_definition"
            && let Some(name) = n.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
            && text.starts_with("test_")
        {
            return true;
        }
        // TestCase サブクラス内
        if n.kind() == "class_definition"
            && let Some(bases) = n.child_by_field_name("superclasses")
            && let Ok(text) = bases.utf8_text(source)
            && text.contains("TestCase")
        {
            return true;
        }
        // pytest decorator
        if n.kind() == "decorated_definition" {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "decorator"
                    && let Ok(text) = child.utf8_text(source)
                    && (text.contains("pytest.fixture") || text.contains("pytest.mark"))
                {
                    return true;
                }
            }
        }
        current = n.parent();
    }
    false
}

/// JS/TS/TSX: `test()`/`it()`/`describe()` call 内
fn is_js_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "call_expression"
            && let Some(func) = n.child_by_field_name("function")
            && let Ok(name) = func.utf8_text(source)
            && matches!(
                name,
                "test" | "it" | "describe" | "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
            )
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin: `@Test` アノテーション
fn is_jvm_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        let is_method = matches!(
            n.kind(),
            "method_declaration" | "function_declaration" | "constructor_declaration"
        );
        if is_method && has_jvm_annotation(n, source, "Test") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin のアノテーション（`@Test` 等）の存在チェック。
fn has_jvm_annotation(node: tree_sitter::Node, source: &[u8], name: &str) -> bool {
    // Java: modifiers > marker_annotation > identifier
    // Kotlin: modifiers > annotation > ... > simple_identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "modifiers" | "marker_annotation" | "annotation"
        ) && let Ok(text) = child.utf8_text(source)
            && text.contains(name)
        {
            return true;
        }
    }
    // prev sibling check (annotation が method の前に独立ノードとして存在する場合)
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if matches!(p.kind(), "marker_annotation" | "annotation") {
            if let Ok(text) = p.utf8_text(source)
                && text.contains(name)
            {
                return true;
            }
            prev = p.prev_named_sibling();
        } else if p.kind().contains("comment") {
            prev = p.prev_named_sibling();
        } else {
            break;
        }
    }
    false
}

/// C#: `[Test]`/`[Fact]`/`[TestMethod]` アトリビュート
fn is_csharp_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "method_declaration"
            && has_csharp_attribute(n, source, &["Test", "TestMethod", "Fact", "Theory"])
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// C# のアトリビュート（`[Test]` 等）の存在チェック。
fn has_csharp_attribute(node: tree_sitter::Node, source: &[u8], names: &[&str]) -> bool {
    // attribute_list が method_declaration の子ノードとして存在
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list"
            && let Ok(text) = child.utf8_text(source)
        {
            for name in names {
                if text.contains(name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Ruby: `describe`/`it` call 内 / `test_*` メソッド名
fn is_ruby_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };

    let mut current = Some(node);
    while let Some(n) = current {
        // RSpec: describe/it/before/after call
        if n.kind() == "call"
            && let Some(method) = n.child_by_field_name("method")
            && let Ok(name) = method.utf8_text(source)
            && matches!(name, "describe" | "context" | "it" | "before" | "after")
        {
            return true;
        }
        // Minitest: test_* メソッド
        if n.kind() == "method"
            && let Some(name) = n.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
            && (text.starts_with("test_") || text == "setup" || text == "teardown")
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// ノードの前に指定テキストを含む attribute_item 兄弟が存在するか確認する。
pub(crate) fn has_attribute_text(node: tree_sitter::Node, source: &[u8], pattern: &str) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if let Ok(text) = p.utf8_text(source)
                    && text.contains(pattern)
                {
                    return true;
                }
                prev = p.prev_named_sibling();
            }
            "line_comment" | "block_comment" => {
                prev = p.prev_named_sibling();
            }
            _ => break,
        }
    }
    false
}

/// ターゲットファイルの指定行/列の参照がテストコンテキスト内にあるか確認する。
///
/// ターゲットファイルをオンデマンドでパースし、再パースを避けるためキャッシュする。
/// `#[cfg(test)]` モジュールや `#[test]` 関数内の影響を受ける呼び出し元を除外する。
pub(crate) fn is_ref_in_target_test_context(
    path: &str,
    line: usize,
    column: usize,
    cache: &mut HashMap<String, Option<ParsedFile>>,
) -> bool {
    let entry = cache.entry(path.to_string()).or_insert_with(|| {
        let utf8_path = Utf8Path::new(path);
        let source = parser::read_file(utf8_path).ok()?;
        let source_vec = source.as_bytes().to_vec();
        let (tree, lang_id) = parser::parse_file(utf8_path, &source).ok()?;
        Some((tree, source_vec, lang_id))
    });

    let Some((tree, source, lang_id)) = entry else {
        return false;
    };

    let range = crate::models::location::Range {
        start: crate::models::location::Point { line, column },
        end: crate::models::location::Point { line, column },
    };

    is_in_test_context(tree.root_node(), source, &range, *lang_id, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go のテストファイルパターン (*_test.go) を検出する
    #[test]
    fn is_test_file_path_go() {
        assert!(is_test_file_path("pkg/handler_test.go"));
    }

    // JS/TS の spec ファイルパターン (*.spec.ts) を検出する
    #[test]
    fn is_test_file_path_js_spec() {
        assert!(is_test_file_path("src/app.spec.ts"));
    }

    // Python のテストファイルパターン (test_*.py) を検出する
    #[test]
    fn is_test_file_path_python() {
        assert!(is_test_file_path("tests/test_main.py"));
    }

    // Ruby の spec ファイルパターン (*_spec.rb) を検出する
    #[test]
    fn is_test_file_path_ruby() {
        assert!(is_test_file_path("spec/model_spec.rb"));
    }

    // 通常のソースファイルはテストファイルと判定されない
    #[test]
    fn is_test_file_path_normal() {
        assert!(!is_test_file_path("src/main.rs"));
        assert!(!is_test_file_path("lib/handler.py"));
    }
}
