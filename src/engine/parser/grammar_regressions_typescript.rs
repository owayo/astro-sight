use super::parse_source;
use crate::language::LangId;
use tree_sitter::{InputEdit, Node, Parser, Point, Query, Tree};

const TYPESCRIPT_TYPE_QUERY_SOURCE: &str =
    include_str!("../../../tests/fixtures/typescript_type_query.ts");
const TSX_TYPE_QUERY_SOURCE: &str =
    include_str!("../../../tests/fixtures/typescript_type_query.tsx");

fn typescript_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    nodes
}

fn assert_typescript_parse_clean(source: &[u8], lang: LangId) -> Tree {
    let tree = parse_source(source, lang).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "有効な型引数の type query が構文エラーになった ({lang:?}): {}",
        tree.root_node().to_sexp()
    );
    assert!(
        typescript_nodes(tree.root_node())
            .iter()
            .all(|node| { !node.is_error() && !node.is_missing() })
    );
    tree
}

fn typescript_node_with_text<'a>(
    root: Node<'a>,
    source: &[u8],
    kind: &str,
    text: &str,
) -> Node<'a> {
    typescript_nodes(root)
        .into_iter()
        .find(|node| node.kind() == kind && node.utf8_text(source).unwrap() == text)
        .unwrap_or_else(|| panic!("ノードがない: {kind} {text}: {}", root.to_sexp()))
}

fn typescript_point_at(source: &[u8], byte: usize) -> Point {
    let prefix = &source[..byte];
    Point::new(
        prefix.iter().filter(|&&value| value == b'\n').count(),
        prefix
            .iter()
            .rposition(|&value| value == b'\n')
            .map_or(byte, |line| byte - line - 1),
    )
}

fn assert_typescript_import_type_argument(call: Node<'_>, source: &[u8]) {
    let type_arguments = call.child_by_field_name("type_arguments").unwrap();
    assert_eq!(type_arguments.kind(), "type_arguments");
    assert_eq!(
        type_arguments.utf8_text(source).unwrap(),
        "<typeof import(\"node:fs\")>"
    );
    assert_eq!(type_arguments.named_child_count(), 1);
    let query = type_arguments.named_child(0).unwrap();
    assert_eq!(query.kind(), "type_query");
    assert_eq!(
        query.utf8_text(source).unwrap(),
        "typeof import(\"node:fs\")"
    );
    assert_eq!(query.named_child_count(), 1);
    let import = query.named_child(0).unwrap();
    assert_eq!(import.kind(), "call_expression");
    assert_eq!(
        import.child_by_field_name("function").unwrap().kind(),
        "import"
    );
    let arguments = import.child_by_field_name("arguments").unwrap();
    assert_eq!(arguments.named_child_count(), 1);
    let specifier = arguments.named_child(0).unwrap();
    assert_eq!(specifier.kind(), "string");
    assert_eq!(specifier.utf8_text(source).unwrap(), "\"node:fs\"");
    assert_eq!(
        call.child_by_field_name("arguments")
            .unwrap()
            .named_child_count(),
        0
    );
}

fn assert_typescript_reported_case(source: &str, lang: LangId) {
    let bytes = source.as_bytes();
    let tree = assert_typescript_parse_clean(bytes, lang);
    assert_eq!(tree.root_node().byte_range(), 0..bytes.len());
    let call = typescript_nodes(tree.root_node())
        .into_iter()
        .find(|node| {
            node.kind() == "call_expression" && node.child_by_field_name("type_arguments").is_some()
        })
        .unwrap();
    assert_typescript_import_type_argument(call, bytes);
    let callee = typescript_nodes(call.child_by_field_name("function").unwrap())
        .into_iter()
        .find(|node| node.kind() == "identifier")
        .unwrap();
    assert_eq!(callee.utf8_text(bytes).unwrap(), "importOriginal");
    assert_eq!(callee.start_position(), Point::new(1, 23));
    assert_eq!(callee.end_position(), Point::new(1, 37));
    assert_eq!(call.end_position(), Point::new(1, 65));
    let arguments = call.child_by_field_name("type_arguments").unwrap();
    assert_eq!(arguments.start_position(), Point::new(1, 37));
    assert_eq!(arguments.end_position(), Point::new(1, 63));
    for node in typescript_nodes(call) {
        assert_eq!(
            node.start_position(),
            typescript_point_at(bytes, node.start_byte())
        );
        assert_eq!(
            node.end_position(),
            typescript_point_at(bytes, node.end_byte())
        );
    }
    for (name, line) in [("read", 6), ("close", 7)] {
        let method = typescript_nodes(tree.root_node())
            .into_iter()
            .find(|node| {
                node.kind() == "method_definition"
                    && node
                        .child_by_field_name("name")
                        .unwrap()
                        .utf8_text(bytes)
                        .unwrap()
                        == name
            })
            .unwrap();
        assert_eq!(method.parent().unwrap().kind(), "object");
        assert_eq!(method.start_position(), Point::new(line, 2));
        assert_eq!(
            method.child_by_field_name("body").unwrap().kind(),
            "statement_block"
        );
    }
    let load = typescript_nodes(tree.root_node())
        .into_iter()
        .find(|node| {
            node.kind() == "function_declaration"
                && node
                    .child_by_field_name("name")
                    .unwrap()
                    .utf8_text(bytes)
                    .unwrap()
                    == "load"
        })
        .unwrap();
    assert_eq!(load.end_position(), Point::new(3, 1));
    let returned =
        typescript_node_with_text(tree.root_node(), bytes, "return_statement", "return actual");
    assert_eq!(returned.start_position(), Point::new(2, 2));
    if lang == LangId::Tsx {
        typescript_node_with_text(
            tree.root_node(),
            bytes,
            "jsx_element",
            "<section>{handlers.close()}</section>",
        );
    }
}

#[test]
fn typescript_reported_type_query_generic_call() {
    assert_typescript_reported_case(TYPESCRIPT_TYPE_QUERY_SOURCE, LangId::Typescript);
}

#[test]
fn typescript_reported_type_query_generic_call_tsx() {
    assert_typescript_reported_case(TSX_TYPE_QUERY_SOURCE, LangId::Tsx);
}

#[test]
fn typescript_type_query_call_variants_keep_type_and_expression_boundaries() {
    for (expression, callee_kind, awaited) in [
        (
            "importOriginal<typeof import(\"node:fs\")>()",
            "identifier",
            false,
        ),
        (
            "(importOriginal<typeof import(\"node:fs\")>())",
            "identifier",
            false,
        ),
        (
            "await (importOriginal<typeof import(\"node:fs\")>())",
            "identifier",
            true,
        ),
        (
            "source.load<typeof import(\"node:fs\")>()",
            "member_expression",
            false,
        ),
        (
            "await (source.load<typeof import(\"node:fs\")>())",
            "member_expression",
            true,
        ),
    ] {
        let source = format!("async function load() {{ return {expression}; }}");
        let mut previous = None;
        for lang in [LangId::Typescript, LangId::Tsx] {
            let tree = assert_typescript_parse_clean(source.as_bytes(), lang);
            let call = typescript_nodes(tree.root_node())
                .into_iter()
                .find(|node| {
                    node.kind() == "call_expression"
                        && node.child_by_field_name("type_arguments").is_some()
                })
                .unwrap();
            assert_typescript_import_type_argument(call, source.as_bytes());
            assert_eq!(
                call.child_by_field_name("function").unwrap().kind(),
                callee_kind
            );
            if awaited {
                let grouped = call.parent().unwrap();
                assert_eq!(grouped.kind(), "parenthesized_expression");
                assert_eq!(grouped.parent().unwrap().kind(), "await_expression");
            }
            let nodes = typescript_nodes(tree.root_node());
            assert_eq!(
                nodes
                    .iter()
                    .filter(|node| node.kind() == "await_expression")
                    .count(),
                usize::from(awaited)
            );
            assert!(nodes.iter().all(
                |node| node.kind() != "binary_expression" && node.kind() != "unary_expression"
            ));
            let sexp = tree.root_node().to_sexp();
            if let Some(previous) = &previous {
                assert_eq!(&sexp, previous);
            }
            previous = Some(sexp);
        }
    }
}

#[test]
fn typescript_type_query_expression_controls_keep_exact_named_trees() {
    for (source, expected) in [
        (
            "const comparison = left < middle > right;",
            "(program (lexical_declaration (variable_declarator name: (identifier) value: (binary_expression left: (binary_expression left: (identifier) right: (identifier)) right: (identifier)))))",
        ),
        (
            "const runtime = import(\"node:fs\");",
            "(program (lexical_declaration (variable_declarator name: (identifier) value: (call_expression function: (import) arguments: (arguments (string (string_fragment)))))))",
        ),
        (
            "const runtimeType = typeof value;",
            "(program (lexical_declaration (variable_declarator name: (identifier) value: (unary_expression argument: (identifier)))))",
        ),
        (
            "type Module = typeof import(\"node:fs\");",
            "(program (type_alias_declaration name: (type_identifier) value: (type_query (call_expression function: (import) arguments: (arguments (string (string_fragment)))))))",
        ),
    ] {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let tree = assert_typescript_parse_clean(source.as_bytes(), lang);
            assert_eq!(tree.root_node().to_sexp(), expected, "{lang:?}: {source}");
        }
    }
}

#[test]
fn typescript_type_query_generic_controls_keep_call_shapes() {
    for (source, kind, text, arguments) in [
        (
            "const result = call<Item>();",
            "call_expression",
            "call<Item>()",
            true,
        ),
        (
            "const result = new Box<Item>();",
            "new_expression",
            "new Box<Item>()",
            true,
        ),
        (
            "const result = call?.<Item>();",
            "call_expression",
            "call?.<Item>()",
            true,
        ),
        (
            "const result = call<Item>;",
            "instantiation_expression",
            "call<Item>",
            false,
        ),
        (
            "const result = call<Map<string, Array<number>>>();",
            "call_expression",
            "call<Map<string, Array<number>>>()",
            true,
        ),
    ] {
        let mut previous = None;
        for lang in [LangId::Typescript, LangId::Tsx] {
            let tree = assert_typescript_parse_clean(source.as_bytes(), lang);
            let node = typescript_node_with_text(tree.root_node(), source.as_bytes(), kind, text);
            assert_eq!(
                node.child_by_field_name("type_arguments").unwrap().kind(),
                "type_arguments"
            );
            assert_eq!(node.child_by_field_name("arguments").is_some(), arguments);
            assert!(
                typescript_nodes(tree.root_node())
                    .iter()
                    .all(|node| node.kind() != "binary_expression" && node.kind() != "type_query")
            );
            let sexp = tree.root_node().to_sexp();
            if let Some(previous) = &previous {
                assert_eq!(&sexp, previous);
            }
            previous = Some(sexp);
        }
    }
}

#[test]
fn typescript_type_query_generic_arrows_and_jsx_keep_distinct_nodes() {
    let arrow_source = "const identity = <T,>(value: T) => value;";
    for lang in [LangId::Typescript, LangId::Tsx] {
        let tree = assert_typescript_parse_clean(arrow_source.as_bytes(), lang);
        let arrow = typescript_node_with_text(
            tree.root_node(),
            arrow_source.as_bytes(),
            "arrow_function",
            "<T,>(value: T) => value",
        );
        assert_eq!(
            arrow.child_by_field_name("type_parameters").unwrap().kind(),
            "type_parameters"
        );
        assert!(
            typescript_nodes(tree.root_node())
                .iter()
                .all(|node| !node.kind().starts_with("jsx_"))
        );
    }
    let source = "const view = <Panel value={call<Item>()} />;";
    let tree = assert_typescript_parse_clean(source.as_bytes(), LangId::Tsx);
    typescript_node_with_text(
        tree.root_node(),
        source.as_bytes(),
        "jsx_self_closing_element",
        "<Panel value={call<Item>()} />",
    );
    let call = typescript_node_with_text(
        tree.root_node(),
        source.as_bytes(),
        "call_expression",
        "call<Item>()",
    );
    assert_eq!(call.parent().unwrap().kind(), "jsx_expression");
    assert_eq!(
        call.child_by_field_name("type_arguments")
            .unwrap()
            .utf8_text(source.as_bytes())
            .unwrap(),
        "<Item>"
    );
}

#[test]
fn typescript_type_query_preserves_unicode_crlf_and_raw_ranges() {
    let source = format!(
        "// 名前🙂\r\n{}",
        TYPESCRIPT_TYPE_QUERY_SOURCE
            .replace(
                "await importOriginal<typeof import(\"node:fs\")>()",
                "await (/* 名前🙂 */ importOriginal<typeof import(\"node:fs\")>())"
            )
            .replace('\n', "\r\n")
    );
    let bytes = source.as_bytes();
    for lang in [LangId::Typescript, LangId::Tsx] {
        let tree = assert_typescript_parse_clean(bytes, lang);
        let text = "importOriginal<typeof import(\"node:fs\")>()";
        let call = typescript_node_with_text(tree.root_node(), bytes, "call_expression", text);
        let byte = source.find(text).unwrap();
        assert_eq!(call.byte_range(), byte..byte + text.len());
        assert_eq!(
            call.start_position(),
            Point::new(2, "  const actual = await (/* 名前🙂 */ ".len())
        );
        assert_typescript_import_type_argument(call, bytes);
        for node in typescript_nodes(tree.root_node()) {
            assert_eq!(
                node.start_position(),
                typescript_point_at(bytes, node.start_byte())
            );
            assert_eq!(
                node.end_position(),
                typescript_point_at(bytes, node.end_byte())
            );
            assert_eq!(
                node.utf8_text(bytes).unwrap().as_bytes(),
                &bytes[node.byte_range()]
            );
        }
    }
}

#[test]
fn typescript_type_query_malformed_input_keeps_errors_and_parser_pool_recovers() {
    for source in [
        "async function load() { return await call<typeof import(@)>(); }\nexport function later() {}",
        "function load() { return call<typeof import(\"node:fs\") @>(); }\nexport function later() {}",
        "function load() { return call<typeof import(\"node:fs\")>();",
    ] {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let broken = parse_source(source.as_bytes(), lang).unwrap();
            assert!(broken.root_node().has_error(), "{source}");
            assert!(
                typescript_nodes(broken.root_node())
                    .iter()
                    .any(|node| node.is_error() || node.is_missing())
            );
            if source.contains("later") {
                typescript_node_with_text(
                    broken.root_node(),
                    source.as_bytes(),
                    "function_declaration",
                    "function later() {}",
                );
            }
            let valid =
                assert_typescript_parse_clean(TYPESCRIPT_TYPE_QUERY_SOURCE.as_bytes(), lang);
            let mut parser = Parser::new();
            parser.set_language(&lang.ts_language()).unwrap();
            let fresh = parser.parse(TYPESCRIPT_TYPE_QUERY_SOURCE, None).unwrap();
            assert_eq!(valid.root_node().to_sexp(), fresh.root_node().to_sexp());
        }
    }
}

#[test]
fn typescript_type_query_incremental_parse_recovers_invalid_to_valid() {
    let valid = "async function load() { return await (call<typeof import(\"node:fs\")>()); }";
    let broken = valid.replace("\"node:fs\"", "@");
    let start = broken.find('@').unwrap();
    for lang in [LangId::Typescript, LangId::Tsx] {
        let mut parser = Parser::new();
        parser.set_language(&lang.ts_language()).unwrap();
        let mut old_tree = parser.parse(&broken, None).unwrap();
        assert!(old_tree.root_node().has_error());
        old_tree.edit(&InputEdit {
            start_byte: start,
            old_end_byte: start + 1,
            new_end_byte: start + "\"node:fs\"".len(),
            start_position: Point::new(0, start),
            old_end_position: Point::new(0, start + 1),
            new_end_position: Point::new(0, start + "\"node:fs\"".len()),
        });
        let incremental = parser.parse(valid, Some(&old_tree)).unwrap();
        assert!(!incremental.root_node().has_error());
        let fresh = parser.parse(valid, None).unwrap();
        assert_eq!(
            incremental.root_node().to_sexp(),
            fresh.root_node().to_sexp()
        );
        let call = typescript_node_with_text(
            incremental.root_node(),
            valid.as_bytes(),
            "call_expression",
            "call<typeof import(\"node:fs\")>()",
        );
        assert_typescript_import_type_argument(call, valid.as_bytes());
    }
}

#[test]
fn typescript_type_query_node_and_field_contracts_exist() {
    for lang in [LangId::Typescript, LangId::Tsx] {
        Query::new(&lang.ts_language(), "(call_expression function: (identifier) type_arguments: (type_arguments (type_query (call_expression function: (import) arguments: (arguments (string))))) arguments: (arguments))").unwrap();
        Query::new(&lang.ts_language(), "(await_expression (call_expression))").unwrap();
    }
}

#[test]
fn typescript_await_and_tagged_template_no_regression_vs_baseline() {
    let source = "async function bare() { return await call<Item>(); }\nasync function grouped() { return await (call<Item>()); }\nconst tagged = tag<Item>`value`;\n";
    // 旧文法の未解決構造を今回の修正で変えない。
    let expected = "(program (function_declaration name: (identifier) parameters: (formal_parameters) body: (statement_block (return_statement (call_expression function: (await_expression (identifier)) type_arguments: (type_arguments (type_identifier)) arguments: (arguments))))) (function_declaration name: (identifier) parameters: (formal_parameters) body: (statement_block (return_statement (await_expression (parenthesized_expression (call_expression function: (identifier) type_arguments: (type_arguments (type_identifier)) arguments: (arguments))))))) (lexical_declaration (variable_declarator name: (identifier) value: (binary_expression left: (binary_expression left: (identifier) right: (identifier)) right: (template_string (string_fragment))))))";
    for lang in [LangId::Typescript, LangId::Tsx] {
        let tree = assert_typescript_parse_clean(source.as_bytes(), lang);
        assert_eq!(tree.root_node().to_sexp(), expected);
    }
}
