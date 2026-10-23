use super::parse_source;
use crate::language::LangId;
use tree_sitter::{Node, Point, Tree};

const SOURCE: &str = include_str!("../../../tests/fixtures/rust_unicode_escapes.rs");

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
    let tree = parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let root = tree.root_node();
    assert!(!root.has_error(), "{source}\n{}", root.to_sexp());
    assert_eq!(root.byte_range(), 0..source.len());
    assert!(
        nodes(root)
            .iter()
            .all(|node| !node.is_error() && !node.is_missing()),
        "{source}\n{}",
        root.to_sexp()
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
fn rust_unicode_separators_preserve_following_declarations_and_byte_positions() {
    for source in [SOURCE.to_string(), SOURCE.replace('\n', "\r\n")] {
        let tree = clean(&source);
        let mut cursor = tree.root_node().walk();
        assert_eq!(
            tree.root_node()
                .named_children(&mut cursor)
                .map(|node| node.kind())
                .collect::<Vec<_>>(),
            ["const_item", "function_item", "function_item"]
        );
        assert_eq!(
            texts(&tree, &source, "escape_sequence"),
            [r"\u{4f_dd}", r"\u{5b__58_}"]
        );
        let call = nodes(tree.root_node())
            .into_iter()
            .find(|node| {
                node.kind() == "call_expression"
                    && node.utf8_text(source.as_bytes()).unwrap() == "after_unicode()"
            })
            .unwrap();
        let offset = source.rfind("after_unicode()").unwrap();
        let main_line = source.lines().nth(2).unwrap();
        let column = main_line.find("after_unicode()").unwrap();
        assert!(column > main_line[..column].chars().count());
        assert_eq!(call.byte_range(), offset..offset + "after_unicode()".len());
        assert_eq!(call.start_position(), Point::new(2, column));
        assert_eq!(
            call.end_position(),
            Point::new(2, column + "after_unicode()".len())
        );
        let function = call.child_by_field_name("function").unwrap();
        assert_eq!(
            function.utf8_text(source.as_bytes()).unwrap(),
            "after_unicode"
        );
        assert_eq!(
            function.byte_range(),
            offset..offset + "after_unicode".len()
        );
    }
}

#[test]
fn rust_unicode_separators_in_string_character_and_c_string_literals() {
    for digits in ["0", "0_", "4f_dd", "4F__DD__", "00_0041_", "10_FFFF_"] {
        let escape = format!(r"\u{{{digits}}}");
        let mut literals = vec![
            (format!("\"{escape}\""), "string_literal"),
            (format!("'{escape}'"), "char_literal"),
        ];
        // C strings cannot contain NUL; use only valid nonzero scalar values.
        if !digits.starts_with("0_") && digits != "0" {
            literals.push((format!("c\"{escape}\""), "string_literal"));
        }
        for (literal, kind) in literals {
            let source = format!(
                "fn before() {{}}\nfn sample() {{ let value = {literal}; }}\nfn after() {{}}\n"
            );
            let tree = clean(&source);
            assert_eq!(texts(&tree, &source, kind), [literal.as_str()]);
            let functions: Vec<_> = nodes(tree.root_node())
                .into_iter()
                .filter(|node| node.kind() == "function_item")
                .map(|node| {
                    node.child_by_field_name("name")
                        .unwrap()
                        .utf8_text(source.as_bytes())
                        .unwrap()
                })
                .collect();
            assert_eq!(functions, ["before", "sample", "after"]);
            if kind == "string_literal" {
                assert_eq!(texts(&tree, &source, "escape_sequence"), [escape.as_str()]);
            }
        }
    }
}

#[test]
fn rust_unicode_escape_spelling_stays_literal_in_raw_strings_and_after_backslashes() {
    for (literal, kind, escapes) in [
        (r##"r#"\u{4f_dd}"#"##, "raw_string_literal", vec![]),
        (r#""\\u{4f_dd}""#, "string_literal", vec![r"\\"]),
    ] {
        let source = format!("fn sample() {{ let value = {literal}; }}\n");
        let tree = clean(&source);
        assert_eq!(texts(&tree, &source, kind), [literal]);
        assert_eq!(texts(&tree, &source, "escape_sequence"), escapes);
    }
}

#[test]
fn rust_unicode_escapes_still_require_a_leading_hex_digit() {
    for digits in ["_41", "___", ""] {
        for literal in [format!(r#""\u{{{digits}}}""#), format!(r"'\u{{{digits}}}'")] {
            let source = format!("fn sample() {{ let value = {literal}; }}\n");
            let tree = parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert!(tree.root_node().has_error(), "{source}");
        }
    }
}
