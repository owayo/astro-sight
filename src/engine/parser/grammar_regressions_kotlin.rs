use super::parse_source;
use crate::language::LangId;
use tree_sitter::{Node, Tree};

const SOURCE: &str = include_str!("../../../tests/fixtures/kotlin_when_return.kt");

fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = vec![];
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        result.push(node);
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    result.sort_by_key(Node::start_byte);
    result
}

fn clean(source: &str) -> Tree {
    let tree = parse_source(source.as_bytes(), LangId::Kotlin).unwrap();
    assert!(
        nodes(tree.root_node())
            .iter()
            .all(|node| !node.is_error() && !node.is_missing()),
        "{source}\n{}",
        tree.root_node().to_sexp()
    );
    tree
}

fn texts<'a>(tree: &Tree, source: &'a str, kind: &str) -> Vec<&'a str> {
    nodes(tree.root_node())
        .iter()
        .filter(|node| node.kind() == kind)
        .map(|node| node.utf8_text(source.as_bytes()).unwrap())
        .collect()
}

#[test]
fn kotlin_when_return_entries_and_byte_ranges() {
    for source in [SOURCE.to_string(), SOURCE.replace('\n', "\r\n")] {
        let tree = clean(&source);
        let entries = texts(&tree, &source, "when_entry");
        assert_eq!(
            entries,
            [
                "offset == 0 -> return offset + 1",
                "(offset and 0xC0) == 0xC0 -> return offset + 2",
                "else -> return offset + 3",
            ]
        );
        assert_eq!(
            texts(&tree, &source, "jump_expression"),
            [
                "return offset + 1",
                "return offset + 2",
                "return offset + 3",
            ]
        );
        assert!(texts(&tree, &source, "call_expression").is_empty());
        let jumps: Vec<_> = nodes(tree.root_node())
            .into_iter()
            .filter(|node| node.kind() == "jump_expression")
            .collect();
        for (node, text) in jumps.iter().zip([
            "return offset + 1",
            "return offset + 2",
            "return offset + 3",
        ]) {
            let start = source.find(text).unwrap();
            assert_eq!(node.byte_range(), start..start + text.len());
        }
        for node in nodes(tree.root_node())
            .into_iter()
            .filter(|node| node.kind() == "when_entry")
        {
            let text = node.utf8_text(source.as_bytes()).unwrap();
            let start = source.find(text).unwrap();
            assert_eq!(node.byte_range(), start..start + text.len());
        }
    }
}

#[test]
fn kotlin_when_separator_ignores_comments_quotes_and_nested_arrows() {
    for condition in [
        "(offset/* misleading ) -> */ and 0xC0) == 0xC0",
        "(offset /* outer /* ) -> */ nested */ and 0xC0) == 0xC0",
        "(offset // misleading ) ->\n and 0xC0) == 0xC0",
        "(\"literal ) ->\").length == offset",
        "(\"\"\"literal ) ->\"\"\").length == offset",
        "(')') == ')'",
        "(`name ) ->`) == offset",
        "(consume { value -> value }) == offset",
        "(offset) == 0\n && (offset) == 1",
        "(offset) == -1",
        "(offset) - 1 == 0",
    ] {
        let source = SOURCE.replace("(offset and 0xC0) == 0xC0", condition);
        let tree = clean(&source);
        assert_eq!(texts(&tree, &source, "when_entry").len(), 3, "{source}");
        assert_eq!(
            texts(&tree, &source, "jump_expression").len(),
            3,
            "{source}"
        );
    }
    for comment in ["/* ) -> */", "/* outer\n /* -> */ end */", "// ) ->\n"] {
        let source = SOURCE
            .replace("        (offset", &format!("        {comment} (offset"))
            .replace("else ->", "else /* -> */ ->");
        let tree = clean(&source);
        assert_eq!(texts(&tree, &source, "when_entry").len(), 3);
    }
}

#[test]
fn kotlin_when_separator_keeps_expression_continuations() {
    for (body, kind, expected) in [
        (
            "return consume\n(offset)",
            "call_expression",
            "consume\n(offset)",
        ),
        (
            "return consume(\n offset\n)",
            "call_expression",
            "consume(\n offset\n)",
        ),
        (
            "return offset and\n 0xC0",
            "infix_expression",
            "offset and\n 0xC0",
        ),
        ("return offset +\n 1", "additive_expression", "offset +\n 1"),
        (
            "return if (flag) 1\n else 2",
            "if_expression",
            "if (flag) 1\n else 2",
        ),
        (
            "return consume\n(offset) // not a branch\n",
            "call_expression",
            "consume\n(offset)",
        ),
        (
            "return consume\n(offset) /* ) -> */\n",
            "call_expression",
            "consume\n(offset)",
        ),
    ] {
        let source = format!(
            "fun f(offset: Int, flag: Boolean): Int {{\n when {{\n flag -> {body}\n (offset) == 2 -> return 2\n else -> return 3\n }}\n}}\n"
        );
        let tree = clean(&source);
        assert_eq!(texts(&tree, &source, "when_entry").len(), 3, "{source}");
        assert!(
            texts(&tree, &source, kind).contains(&expected),
            "{source}\n{}",
            tree.root_node().to_sexp()
        );
    }
    let source = "fun f(offset: Int): Int { when (offset) {\n 0 -> return consume\n(offset)\n -1 -> return 2\n else -> return 3\n } }";
    let tree = clean(source);
    assert_eq!(texts(&tree, source, "when_entry").len(), 3);
    assert_eq!(
        texts(&tree, source, "call_expression"),
        ["consume\n(offset)"]
    );
}

#[test]
fn kotlin_when_separator_keeps_malformed_diagnostics() {
    for source in [
        SOURCE.replace("== 0xC0 ->", "== 0xC0"),
        SOURCE.replace("return offset + 1", "return offset +"),
        SOURCE.replace("else ->", "else"),
        SOURCE.replace("(offset and 0xC0)", "(offset /* unclosed ->"),
    ] {
        let tree = parse_source(source.as_bytes(), LangId::Kotlin).unwrap();
        assert!(
            tree.root_node().has_error(),
            "{source}\n{}",
            tree.root_node().to_sexp()
        );
    }
}

#[test]
fn kotlin_when_separator_handles_subjects_jumps_and_nested_when() {
    for source in [
        "fun f(x: Int): Int { when (x) { 0 -> return 1\n (1) -> return 2\n else -> return 3 } }",
        "fun f(x: Int): Int { when { x == 0 -> throw Error()\n (x) == 1 -> return 2\n else -> return 3 } }",
        "fun f(x: Int) { while (true) { when { x == 0 -> break\n (x) == 1 -> continue\n else -> break } } }",
        "fun f(x: Int) { run label@ { when { x == 0 -> return@label 1\n (x) == 1 -> return@label 2\n else -> return@label 3 } } }",
        "fun f(x: Int) = when { x == 0 -> x + 1\n (x) == 1 -> x + 2\n else -> x + 3 }",
        "fun f(x: Int) { when { x == 0 -> return\n (x) == 1 -> return\n else -> return } }",
        "fun f(x: Int) { when { x == 0 -> return; (x) == 1 -> return; else -> return } }",
        "fun f(x: Int) { when { x == 0 -> { return }\n (x) == 1 -> { return }\n else -> { return } } }",
        "fun f(x: Int) { when { x == 0 -> return\n (x) == 1 -> return } }",
    ] {
        let tree = clean(source);
        let expected = source.matches("->").count();
        assert_eq!(
            texts(&tree, source, "when_entry").len(),
            expected,
            "{source}"
        );
    }
    let source = "fun f(x: Int): Int { when { x == 0 -> return when { x == 1 -> 1\n (x) == 2 -> 2\n else -> 3 }\n (x) == 4 -> return 4\n else -> return 5 } }";
    let tree = clean(source);
    assert_eq!(texts(&tree, source, "when_entry").len(), 6);
}

#[test]
fn kotlin_when_separator_does_not_split_function_type_arrows() {
    let source = "fun f(x: Int): (Int) -> Int {\n when {\n x == 0 -> return make\n (x) as (Int) -> Int\n else -> return { n -> n }\n }\n}";
    let tree = clean(source);
    assert_eq!(texts(&tree, source, "when_entry").len(), 2);
    assert_eq!(
        texts(&tree, source, "jump_expression"),
        ["return make\n (x) as (Int) -> Int", "return { n -> n }"]
    );
    assert_eq!(texts(&tree, source, "call_expression"), ["make\n (x)"]);
    for expression in [
        "(x) as? (Int) -> Int",
        "(x) is (Int) -> Int",
        "(x).choose<(Int) -> Int>()",
        "(x).choose<(Int) -> (Int) -> Int>()",
        "(x) ?: fun(): (Int) -> Int { return { n -> n } }",
        "(x).value-->0",
        "(x) + \"\"\"a\"\"\"\" + \"b -> c\"",
        "(x) + \"\"\"a\"\"\"\"\" + \"b -> c\"",
    ] {
        let source = format!(
            "fun f(x: Int): Any {{\n when {{\n x == 0 -> return make\n {expression}\n else -> return 0\n }}\n}}"
        );
        let tree = clean(&source);
        assert_eq!(texts(&tree, &source, "when_entry").len(), 2, "{source}");
        let expected_returns = if expression.contains("fun()") { 3 } else { 2 };
        assert_eq!(
            texts(&tree, &source, "jump_expression").len(),
            expected_returns,
            "{source}"
        );
        assert!(texts(&tree, &source, "call_expression").contains(&"make\n (x)"));
        let start = source.find("return make").unwrap();
        let end = source.find("\n else").unwrap();
        assert!(
            nodes(tree.root_node())
                .iter()
                .any(|node| node.kind() == "jump_expression" && node.byte_range() == (start..end)),
            "{source}"
        );
    }
}
