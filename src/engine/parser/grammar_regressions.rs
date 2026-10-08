use super::parse_source;
use crate::language::LangId;
use tree_sitter::{InputEdit, Node, Parser, Point, Query, Tree};

const SWIFT_COALESCING_SOURCE: &str =
    include_str!("../../../tests/fixtures/swift_nil_coalescing.swift");

fn swift_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    nodes
}

fn assert_swift_parse_clean(source: &[u8]) -> Tree {
    let tree = parse_source(source, LangId::Swift).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "有効な Swift が構文エラーになった: {}",
        tree.root_node().to_sexp()
    );
    for node in swift_nodes(tree.root_node()) {
        assert!(
            !node.is_error() && !node.is_missing(),
            "不正なノード: {} {:?}",
            node.kind(),
            node.range()
        );
    }
    tree
}

#[test]
fn swift_multiline_conditional_cast_nil_coalescing_preserves_ranges() {
    let source = SWIFT_COALESCING_SOURCE.as_bytes();
    let tree = assert_swift_parse_clean(source);
    let nodes = swift_nodes(tree.root_node());
    assert_eq!(tree.root_node().byte_range(), 0..source.len());

    let mut casts: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| node.kind() == "as_expression")
        .collect();
    casts.sort_by_key(Node::start_byte);
    assert_eq!(casts.len(), 2);
    for (cast, text, start, end) in [
        (
            casts[0],
            "info[\"displayName\"] as? String",
            Point::new(4, 4),
            Point::new(4, 34),
        ),
        (
            casts[1],
            "info[\"name\"] as? String",
            Point::new(5, 7),
            Point::new(5, 30),
        ),
    ] {
        assert_eq!(cast.utf8_text(source).unwrap(), text);
        assert_eq!(cast.start_position(), start);
        assert_eq!(cast.end_position(), end);
        let mut cursor = cast.walk();
        let cast_type = cast.named_children(&mut cursor).last().unwrap();
        assert_eq!(cast_type.kind(), "user_type");
        assert_eq!(cast_type.utf8_text(source).unwrap(), "String");
    }
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node.kind() == "nil_coalescing_expression")
            .count(),
        2
    );
    assert!(nodes.iter().all(|node| node.kind() != "optional_type"));
    let first_coalescing = nodes
        .iter()
        .find(|node| {
            node.kind() == "nil_coalescing_expression" && node.start_position() == Point::new(4, 4)
        })
        .unwrap();
    assert_eq!(
        first_coalescing.child_by_field_name("value").unwrap(),
        casts[0]
    );
    let second_coalescing = first_coalescing.child_by_field_name("if_nil").unwrap();
    assert_eq!(second_coalescing.kind(), "nil_coalescing_expression");
    assert_eq!(
        second_coalescing.child_by_field_name("value").unwrap(),
        casts[1]
    );
    let fallback = second_coalescing.child_by_field_name("if_nil").unwrap();
    assert_eq!(fallback.kind(), "line_string_literal");
    assert_eq!(fallback.utf8_text(source).unwrap(), "\"Example\"");
    assert_eq!(fallback.start_position(), Point::new(6, 7));
    assert_eq!(first_coalescing.end_position(), Point::new(6, 16));

    let after = nodes
        .iter()
        .find(|node| {
            node.kind() == "function_declaration"
                && node
                    .utf8_text(source)
                    .unwrap()
                    .starts_with("func afterChain")
        })
        .unwrap();
    assert_eq!(
        after.start_byte(),
        SWIFT_COALESCING_SOURCE.find("func afterChain").unwrap()
    );
    assert_eq!(after.start_position(), Point::new(10, 0));
    assert_eq!(after.end_position(), Point::new(12, 1));
    assert_eq!(
        after.utf8_text(source).unwrap(),
        "func afterChain(_ info: [String: Any]) -> String {\n  return displayName(info)\n}"
    );
}

fn swift_node_with_text<'tree>(
    tree: &'tree Tree,
    source: &[u8],
    kind: &str,
    text: &str,
) -> Node<'tree> {
    swift_nodes(tree.root_node())
        .into_iter()
        .find(|node| node.kind() == kind && node.utf8_text(source).unwrap() == text)
        .unwrap_or_else(|| {
            panic!(
                "ノードがない: {kind} {text}: {}",
                tree.root_node().to_sexp()
            )
        })
}

fn swift_point_at(source: &[u8], byte: usize) -> Point {
    let prefix = &source[..byte];
    Point::new(
        prefix.iter().filter(|&&b| b == b'\n').count(),
        prefix
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(byte, |newline| byte - newline - 1),
    )
}

#[test]
fn swift_cast_coalescing_controls_keep_operator_boundaries() {
    let same_line = SWIFT_COALESCING_SOURCE.replace("\n    ??", " ??");
    for (source, casts, coalescing, optionals, extra_kind) in [
        (same_line.as_str(), 2, 2, 0, "as_expression"),
        (
            "func plain(_ value: String?) -> String {\n  let text = value\n    ?? \"Example\"\n  return text\n}\n",
            0,
            1,
            1,
            "nil_coalescing_expression",
        ),
        (
            "func force(_ value: Any) -> String { return value as! String }\n",
            1,
            0,
            0,
            "as_operator",
        ),
        (
            "func length(_ value: String?) -> Int { return value?.count ?? 0 }\n",
            0,
            1,
            1,
            "navigation_expression",
        ),
        (
            "func fetch() throws -> String { return \"Example\" }\nlet text = (try? fetch()) ?? \"Example\"\n",
            0,
            1,
            0,
            "try_expression",
        ),
        (
            "func name(_ info: [String: Any]) -> String {\n  let text = info[\"name\"] as? /*cast*/ String /*type*/\n    /*operator*/ ?? /*rhs*/ \"Example\"\n  return text\n}\n",
            1,
            1,
            0,
            "as_operator",
        ),
        (
            "func name(_ info: [String: Any]) -> String {\n  let text = info[\"name\"] as? String // type\n    ?? \"Example\"\n  return text\n}\n",
            1,
            1,
            0,
            "as_operator",
        ),
    ] {
        let tree = assert_swift_parse_clean(source.as_bytes());
        let nodes = swift_nodes(tree.root_node());
        for (kind, count) in [
            ("as_expression", casts),
            ("nil_coalescing_expression", coalescing),
            ("optional_type", optionals),
        ] {
            assert_eq!(
                nodes.iter().filter(|node| node.kind() == kind).count(),
                count,
                "{kind}: {source}: {}",
                tree.root_node().to_sexp()
            );
        }
        assert!(nodes.iter().any(|node| node.kind() == extra_kind));
        for cast in nodes.iter().filter(|node| node.kind() == "as_expression") {
            let cast_type = cast
                .named_children(&mut cast.walk())
                .find(|node| node.kind() == "user_type")
                .unwrap();
            assert_eq!(cast_type.utf8_text(source.as_bytes()).unwrap(), "String");
            assert_eq!(cast.end_byte(), cast_type.end_byte());
            let operator = cast
                .named_children(&mut cast.walk())
                .find(|node| node.kind() == "as_operator")
                .unwrap();
            assert_eq!(
                operator.utf8_text(source.as_bytes()).unwrap(),
                if source.contains("as!") { "as!" } else { "as?" }
            );
        }
    }
}

#[test]
fn swift_optional_annotations_preserve_suffix_shapes() {
    for type_text in [
        "String?",
        "AnyObject??",
        "Array<String>?",
        "[String]?",
        "[String: Int]?",
        "(String, Int)?",
        "String.Type?",
    ] {
        let source = format!("let value: {type_text} = nil\n");
        let tree = assert_swift_parse_clean(source.as_bytes());
        let optional = swift_node_with_text(&tree, source.as_bytes(), "optional_type", type_text);
        assert_eq!(optional.start_byte(), source.find(type_text).unwrap());
        assert_eq!(optional.end_byte(), optional.start_byte() + type_text.len());
        let mut cursor = optional.walk();
        let markers: String = optional
            .children(&mut cursor)
            .filter(|node| node.kind() == "?" || node.kind() == "??")
            .map(|node| node.utf8_text(source.as_bytes()).unwrap())
            .collect();
        assert_eq!(
            markers,
            if type_text == "AnyObject??" {
                "??"
            } else {
                "?"
            }
        );
        assert_eq!(
            swift_nodes(tree.root_node())
                .iter()
                .filter(|node| node.kind() == "nil_coalescing_expression")
                .count(),
            0
        );
    }
}

#[test]
fn swift_double_optional_lambda_and_array_keep_suffixes() {
    let source = b"let stringify = { (v: AnyObject??) -> String? in nil }\nlet values: [AnyObject??] = [nil]\n";
    let tree = assert_swift_parse_clean(source);
    let nodes = swift_nodes(tree.root_node());
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node.kind() == "lambda_literal")
            .count(),
        1
    );
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node.kind() == "optional_type"
                && node.utf8_text(source).unwrap() == "AnyObject??")
            .count(),
        2
    );
    swift_node_with_text(&tree, source, "optional_type", "String?");
    swift_node_with_text(&tree, source, "array_type", "[AnyObject??]");
    assert!(
        nodes
            .iter()
            .all(|node| node.kind() != "nil_coalescing_expression")
    );
}

#[test]
fn swift_cast_coalescing_preserves_crlf_and_unicode_ranges() {
    let source = format!(
        "// 名前🙂\r\n{}",
        SWIFT_COALESCING_SOURCE
            .replace(
                "    info[\"displayName\"]",
                "    /* 名前🙂 */ info[\"displayName\"]"
            )
            .replace('\n', "\r\n")
    );
    let bytes = source.as_bytes();
    let tree = assert_swift_parse_clean(bytes);
    assert_eq!(tree.root_node().byte_range(), 0..bytes.len());
    for text in [
        "info[\"displayName\"] as? String",
        "info[\"name\"] as? String",
    ] {
        let cast = swift_node_with_text(&tree, bytes, "as_expression", text);
        let start = source.find(text).unwrap();
        assert_eq!(cast.byte_range(), start..start + text.len());
        assert_eq!(cast.start_position(), swift_point_at(bytes, start));
        assert_eq!(
            cast.end_position(),
            swift_point_at(bytes, start + text.len())
        );
    }
    let after_text =
        "func afterChain(_ info: [String: Any]) -> String {\r\n  return displayName(info)\r\n}";
    let after = swift_node_with_text(&tree, bytes, "function_declaration", after_text);
    assert_eq!(after.start_byte(), source.find("func afterChain").unwrap());
    assert_eq!(after.start_position(), Point::new(11, 0));
    assert_eq!(after.end_position(), Point::new(13, 1));
}

#[test]
fn swift_malformed_cast_coalescing_keeps_diagnostics_and_later_declarations() {
    for statement in ["value as? @", "value ?? @", "value ??? @"] {
        let source = format!(
            "func broken(_ value: Any) {{\n  let text = {statement}\n}}\nfunc afterBroken() -> Int {{ return 1 }}\n"
        );
        let tree = parse_source(source.as_bytes(), LangId::Swift).unwrap();
        assert!(
            tree.root_node().has_error(),
            "{source}: {}",
            tree.root_node().to_sexp()
        );
        assert!(
            swift_nodes(tree.root_node())
                .iter()
                .any(|node| node.is_error() || node.is_missing()),
            "{source}: {}",
            tree.root_node().to_sexp()
        );
        let after = swift_node_with_text(
            &tree,
            source.as_bytes(),
            "function_declaration",
            "func afterBroken() -> Int { return 1 }",
        );
        assert_eq!(after.start_byte(), source.find("func afterBroken").unwrap());
        assert_eq!(after.start_position(), Point::new(3, 0));
    }
}

#[test]
fn swift_incremental_parse_recovers_invalid_to_valid() {
    let valid = SWIFT_COALESCING_SOURCE;
    let invalid = valid.replacen("?? info", "? info", 1);
    let mut parser = Parser::new();
    parser.set_language(&LangId::Swift.ts_language()).unwrap();
    let mut old_tree = parser.parse(invalid.as_bytes(), None).unwrap();
    assert!(old_tree.root_node().has_error());
    let byte = valid.find("?? info").unwrap() + 1;
    old_tree.edit(&InputEdit {
        start_byte: byte,
        old_end_byte: byte,
        new_end_byte: byte + 1,
        start_position: swift_point_at(invalid.as_bytes(), byte),
        old_end_position: swift_point_at(invalid.as_bytes(), byte),
        new_end_position: swift_point_at(valid.as_bytes(), byte + 1),
    });
    let tree = parser.parse(valid.as_bytes(), Some(&old_tree)).unwrap();
    let fresh = assert_swift_parse_clean(valid.as_bytes());
    assert!(!tree.root_node().has_error());
    assert!(
        swift_nodes(tree.root_node())
            .iter()
            .all(|node| !node.is_error() && !node.is_missing())
    );
    assert_eq!(tree.root_node().to_sexp(), fresh.root_node().to_sexp());
    let after = swift_node_with_text(
        &tree,
        valid.as_bytes(),
        "function_declaration",
        "func afterChain(_ info: [String: Any]) -> String {\n  return displayName(info)\n}",
    );
    assert_eq!(after.start_byte(), valid.find("func afterChain").unwrap());
    assert_eq!(after.start_position(), Point::new(10, 0));
}

#[test]
fn swift_parser_pool_recovers_invalid_to_valid() {
    let invalid_source = SWIFT_COALESCING_SOURCE.replacen("?? info", "? info", 1);
    for _ in 0..3 {
        let invalid = parse_source(invalid_source.as_bytes(), LangId::Swift).unwrap();
        assert!(invalid.root_node().has_error());
        assert_swift_parse_clean(SWIFT_COALESCING_SOURCE.as_bytes());
    }
}

#[test]
fn swift_optional_suffix_node_contracts_exist() {
    for query in [
        "(as_expression (as_operator))",
        "(nil_coalescing_expression)",
        "(optional_type \"?\")",
        "(optional_type \"??\")",
        "(lambda_literal)",
        "(array_type)",
        "(try_expression)",
        "(navigation_expression)",
    ] {
        Query::new(&LangId::Swift.ts_language(), query)
            .unwrap_or_else(|error| panic!("{query}: {error}"));
    }
}
