//! 型注釈のない const の静的な object/array の構造を比較する。

use std::collections::HashSet;

use tree_sitter::Node;

use super::js_const::unique_top_level_const;
use super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::language::LangId;

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
enum ScalarKind {
    Number,
    BigInt,
    String,
    Boolean,
    Null,
}

#[derive(Debug, PartialEq, Eq)]
enum StaticShape {
    Scalar(ScalarKind),
    Object(Vec<(String, StaticShape)>),
    Array(Vec<StaticShape>),
    ConstAssert(Box<StaticShape>),
}

#[derive(Debug, PartialEq, Eq)]
struct StaticConstContract {
    prefix: SigTokens,
    shape: StaticShape,
    suffix: SigTokens,
}

pub(super) enum StaticConstChange {
    NotApplicable,
    Unchanged,
    ValueOnly,
}

pub(super) fn classify_static_const_change(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> StaticConstChange {
    if site.kind != "variable" {
        return StaticConstChange::NotApplicable;
    }
    let Some(lang) = site.lang_in(&[LangId::Javascript, LangId::Typescript, LangId::Tsx]) else {
        return StaticConstChange::NotApplicable;
    };
    // container があり得ない候補を読み込み前に除外し、構造の証明は AST に任せる。
    if [site.old_sig, site.new_sig]
        .iter()
        .any(|signature| !signature.contains('{') && !signature.contains('['))
    {
        return StaticConstChange::NotApplicable;
    }
    let Some(source) = sources.get(site) else {
        return StaticConstChange::NotApplicable;
    };
    let Some((old_tree, new_tree)) = source.parse_pair(lang) else {
        return StaticConstChange::NotApplicable;
    };
    let (Some((old, old_value)), Some((new, new_value))) = (
        static_const_contract(old_tree.root_node(), &source.old, site.name, lang),
        static_const_contract(new_tree.root_node(), &source.new, site.name, lang),
    ) else {
        return StaticConstChange::NotApplicable;
    };
    if old != new {
        return StaticConstChange::NotApplicable;
    }
    let (Some(old_tokens), Some(new_tokens)) = (
        primitive_value_tokens(old_value, &source.old),
        primitive_value_tokens(new_value, &source.new),
    ) else {
        return StaticConstChange::NotApplicable;
    };
    if old_tokens == new_tokens {
        StaticConstChange::Unchanged
    } else {
        StaticConstChange::ValueOnly
    }
}

fn static_const_contract<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
    lang: LangId,
) -> Option<(StaticConstContract, Node<'tree>)> {
    // 全木の構文エラー、一意性、const、単一 declarator を共通の証明で確認する。
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    if declarator.child_by_field_name("type").is_some() {
        return None;
    }
    check_candidate(statement, source)?;
    let value = declarator.child_by_field_name("value")?;
    let shape = static_shape(value, source, lang, 0)?;
    if !is_container(&shape) {
        return None;
    }
    Some((
        StaticConstContract {
            prefix: header_tokens(
                statement,
                source,
                statement.start_byte(),
                value.start_byte(),
            )?,
            shape,
            suffix: header_tokens(statement, source, value.end_byte(), statement.end_byte())?,
        },
        value,
    ))
}

fn primitive_value_tokens(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    collect_primitive_value_tokens(node, source, &mut tokens)?;
    Some(tokens)
}

// 構造一致を証明した初期化子だけを走査し、キー・括弧・カンマ・コメントを含めない。
fn collect_primitive_value_tokens(
    node: Node<'_>,
    source: &[u8],
    tokens: &mut Vec<String>,
) -> Option<()> {
    match node.kind() {
        "number" | "true" | "false" | "null" => {
            tokens.push(node.utf8_text(source).ok()?.to_string());
        }
        "string" => {
            let text = node.utf8_text(source).ok()?;
            // escape のない通常文字列だけ、引用符の違いを整形として除く。
            let value = if text.contains('\\') {
                text
            } else {
                text.strip_prefix('\'')
                    .and_then(|text| text.strip_suffix('\''))
                    .or_else(|| {
                        text.strip_prefix('"')
                            .and_then(|text| text.strip_suffix('"'))
                    })
                    .unwrap_or(text)
            };
            tokens.push(value.to_string());
        }
        "unary_expression" => {
            let argument = node.child_by_field_name("argument")?;
            tokens.push(format!("-{}", argument.utf8_text(source).ok()?));
        }
        "object" => {
            let mut cursor = node.walk();
            for property in node
                .named_children(&mut cursor)
                .filter(|node| node.kind() != "comment")
            {
                collect_primitive_value_tokens(
                    property.child_by_field_name("value")?,
                    source,
                    tokens,
                )?;
            }
        }
        "array" => {
            let mut cursor = node.walk();
            for element in node
                .named_children(&mut cursor)
                .filter(|node| node.kind() != "comment")
            {
                collect_primitive_value_tokens(element, source, tokens)?;
            }
        }
        "parenthesized_expression" | "as_expression" => {
            let mut cursor = node.walk();
            let inner = node
                .named_children(&mut cursor)
                .find(|node| node.kind() != "comment")?;
            collect_primitive_value_tokens(inner, source, tokens)?;
        }
        _ => return None,
    }
    Some(())
}

fn header_tokens(node: Node<'_>, source: &[u8], start: usize, end: usize) -> Option<SigTokens> {
    let mut tokens = signature_tokens_in_range(node, source, start, end)?;
    tokens.0.retain(|(kind, _)| kind != "comment");
    Some(tokens)
}

fn is_container(shape: &StaticShape) -> bool {
    match shape {
        StaticShape::Object(_) | StaticShape::Array(_) => true,
        StaticShape::ConstAssert(inner) => is_container(inner),
        StaticShape::Scalar(_) => false,
    }
}

fn tagged_jsdoc(node: Node<'_>, source: &[u8]) -> Option<bool> {
    if node.kind() != "comment" {
        return Some(false);
    }
    let text = node.utf8_text(source).ok()?;
    Some(text.starts_with("/**") && text.contains('@'))
}

fn check_candidate(statement: Node<'_>, source: &[u8]) -> Option<()> {
    let mut count = 0;
    let mut previous = statement.prev_sibling();
    while let Some(comment) = previous.filter(|node| node.kind() == "comment") {
        count += 1;
        if count > MAX_NODES || tagged_jsdoc(comment, source)? {
            return None;
        }
        previous = comment.prev_sibling();
    }
    // 無名 token も数え、巨大な部分木を展開せずに上限で打ち切る。
    let mut cursor = statement.walk();
    loop {
        count += 1;
        let node = cursor.node();
        if count > MAX_NODES || node.is_error() || node.is_missing() || tagged_jsdoc(node, source)?
        {
            return None;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(());
            }
        }
    }
}

fn static_shape(node: Node<'_>, source: &[u8], lang: LangId, depth: usize) -> Option<StaticShape> {
    if depth > MAX_DEPTH {
        return None;
    }
    match node.kind() {
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let mut children = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() != "comment");
            let inner = children.next()?;
            if children.next().is_some() {
                return None;
            }
            static_shape(inner, source, lang, depth + 1)
        }
        "number" => number_shape(node, source),
        "unary_expression" => {
            let operator = node.child_by_field_name("operator")?;
            let argument = node.child_by_field_name("argument")?;
            if operator.utf8_text(source).ok()? != "-" || argument.kind() != "number" {
                return None;
            }
            number_shape(argument, source)
        }
        "string" => Some(StaticShape::Scalar(ScalarKind::String)),
        "true" | "false" => Some(StaticShape::Scalar(ScalarKind::Boolean)),
        "null" => Some(StaticShape::Scalar(ScalarKind::Null)),
        "object" => object_shape(node, source, lang, depth),
        "array" => array_shape(node, source, lang, depth),
        "as_expression" if matches!(lang, LangId::Typescript | LangId::Tsx) => {
            let mut cursor = node.walk();
            let mut children = node
                .children(&mut cursor)
                .filter(|child| child.kind() != "comment");
            let expression = children.next()?;
            let keyword = children.next()?;
            let assertion = children.next()?;
            if keyword.kind() != "as"
                || assertion.kind() != "const"
                || assertion.is_named()
                || children.next().is_some()
            {
                return None;
            }
            Some(StaticShape::ConstAssert(Box::new(static_shape(
                expression,
                source,
                lang,
                depth + 1,
            )?)))
        }
        _ => None,
    }
}

fn number_shape(node: Node<'_>, source: &[u8]) -> Option<StaticShape> {
    let kind = if node.utf8_text(source).ok()?.ends_with('n') {
        ScalarKind::BigInt
    } else {
        ScalarKind::Number
    };
    Some(StaticShape::Scalar(kind))
}

fn object_shape(node: Node<'_>, source: &[u8], lang: LangId, depth: usize) -> Option<StaticShape> {
    let mut properties = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = node.walk();
    for property in node.named_children(&mut cursor) {
        if property.kind() == "comment" {
            continue;
        }
        if property.kind() != "pair" {
            return None;
        }
        let key = property_key(property.child_by_field_name("key")?, source)?;
        if key == "__proto__" || !seen.insert(key.clone()) {
            return None;
        }
        let value = static_shape(
            property.child_by_field_name("value")?,
            source,
            lang,
            depth + 1,
        )?;
        properties.push((key, value));
    }
    Some(StaticShape::Object(properties))
}

fn property_key(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "property_identifier" => {
            let text = node.utf8_text(source).ok()?;
            (!text.contains('\\')).then(|| text.to_string())
        }
        "string" => {
            let mut result = String::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "\"" | "'" => {}
                    "string_fragment" => result.push_str(child.utf8_text(source).ok()?),
                    _ => return None,
                }
            }
            Some(result)
        }
        // 数値キーの Number→String 変換は推測しない。
        _ => None,
    }
}

fn array_shape(node: Node<'_>, source: &[u8], lang: LangId, depth: usize) -> Option<StaticShape> {
    let mut elements = Vec::new();
    let mut expect_element = true;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "[" | "comment" => {}
            "]" => break,
            "," => {
                if expect_element {
                    return None;
                }
                expect_element = true;
            }
            _ => {
                if !expect_element {
                    return None;
                }
                elements.push(static_shape(child, source, lang, depth + 1)?);
                expect_element = false;
            }
        }
    }
    Some(StaticShape::Array(elements))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn contract(source: &str, lang: LangId) -> Option<StaticConstContract> {
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        static_const_contract(tree.root_node(), source.as_bytes(), "VALUE", lang)
            .map(|(contract, _)| contract)
    }

    fn initializer(value: &str, lang: LangId) -> Option<StaticConstContract> {
        contract(&format!("export const VALUE = {value};"), lang)
    }

    #[test]
    fn static_shapes_accept_only_equal_ordered_container_contracts() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for (old, new) in [
                (
                    "{ width: 297, margin: { top: 12, left: 14 } }",
                    "{ width: 300, margin: { top: 10, left: 12 } }",
                ),
                (
                    "{ a: [1, 'a', true, null, -1n] }",
                    "{ 'a': [0x10, 'b\\n', false, null, 2n,] }",
                ),
                ("[1, { a: -2 }, []]", "[3, { a: 4 }, []]"),
                ("({ a: 1 })", "{ a: 2 }"),
            ] {
                let old = initializer(old, lang).unwrap();
                assert_eq!(Some(old), initializer(new, lang), "{lang:?}: {new}");
            }
            assert_eq!(
                contract("export /* ordinary */ const VALUE = { a: 1 };", lang),
                contract("export const VALUE /* changed */ = { a: 2 };", lang)
            );
        }
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (old, new) in [
                ("{ a: [1, 'a'] } as const", "{ a: [2, 'b'] } as const"),
                ("{ a: -1 as const }", "{ a: 2 as const }"),
            ] {
                assert_eq!(
                    initializer(old, lang).unwrap(),
                    initializer(new, lang).unwrap()
                );
            }
        }
        let fixture = include_str!("../../../tests/fixtures/static_const_shapes.ts");
        let tree = parser::parse_source(fixture.as_bytes(), LangId::Typescript).unwrap();
        assert!(
            static_const_contract(
                tree.root_node(),
                fixture.as_bytes(),
                "SHAPES",
                LangId::Typescript
            )
            .is_some()
        );
    }

    #[test]
    fn static_shapes_value_tokens_ignore_formatting_and_plain_string_quotes() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let values = |value: &str| {
                let source = format!("export const VALUE = {value};");
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let (_, initializer) =
                    static_const_contract(tree.root_node(), source.as_bytes(), "VALUE", lang)
                        .unwrap();
                primitive_value_tokens(initializer, source.as_bytes()).unwrap()
            };
            let original = "{ a: [- 1, \"a\", true, null], b: { n: 2n } }";
            let formatted = "({ \"a\":[-1,(\"a\"),true,null,], /* ordinary */ \"b\":{n:2n,},})";
            assert_eq!(values(original), vec!["-1", "a", "true", "null", "2n"]);
            assert_eq!(values(original), values(formatted), "{lang:?}");
            assert_eq!(values("['a']"), values("[\"a\"]"), "{lang:?}");
            for (old, new) in [
                ("[0x10]", "[16]"),
                ("['a']", "['b']"),
                ("['a\\n']", "[\"a\\n\"]"),
                ("[-1]", "[1]"),
                ("[1n]", "[2n]"),
                ("[true]", "[false]"),
                ("['a b']", "['a  b']"),
            ] {
                assert_ne!(values(old), values(new), "{lang:?}: {old} -> {new}");
            }
            if lang != LangId::Javascript {
                assert_eq!(
                    values("([1 as const, { a: ('x'), }]) as const"),
                    values("[1 as const,{a:'x'}] as const")
                );
            }
        }
    }

    #[test]
    fn static_shapes_reject_dynamic_or_ambiguous_values() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for value in [
                "{ ...extra, a: 1 }",
                "{ [key]: 1 }",
                "{ a }",
                "{ fn() {} }",
                "{ get a() { return 1; } }",
                "{ a: () => 1 }",
                "{ a: function() {} }",
                "{ a: getValue() }",
                "{ a: value }",
                "{ a: undefined }",
                "{ a: NaN }",
                "{ a: Infinity }",
                "{ a: `text` }",
                "{ a: /x/ }",
                "{ a: +1 }",
                "{ a: -(1) }",
                "{ a: - -1 }",
                "{ a: 1 + 2 }",
                "{ a: 1, 'a': 2 }",
                "{ __proto__: null }",
                "{ '__proto__': { a: 1 } }",
                "{ '\\u0061': 1 }",
                "{ \\u0061: 1 }",
                "{ 1: 1 }",
                "[1,,2]",
                "[,]",
                "[,1]",
                "[...extra]",
                "1",
                "'text'",
                "true",
                "null",
            ] {
                assert!(initializer(value, lang).is_none(), "{lang:?}: {value}");
            }
        }
        for lang in [LangId::Typescript, LangId::Tsx] {
            for value in [
                "{ a: 1 } as Shape",
                "{ a: 1 } satisfies Shape",
                "({ a: 1 } as const) satisfies Shape",
                "({ a: 1 })!",
                "1 as const",
            ] {
                assert!(initializer(value, lang).is_none(), "{value}");
            }
        }
        assert!(initializer("<const>{ a: 1 }", LangId::Typescript).is_none());
    }

    #[test]
    fn static_shapes_preserve_keys_leaf_types_arrays_and_assertion_positions() {
        for (old, new) in [
            ("{ a: 1 }", "{ b: 2 }"),
            ("{ a: 1 }", "{ a: 2, b: 3 }"),
            ("{ a: 1, b: 2 }", "{ b: 3, a: 4 }"),
            ("{ a: 1 }", "{ a: '2' }"),
            ("{ a: 1 }", "{ a: 2n }"),
            ("{ a: null }", "{ a: false }"),
            ("[1, 'a']", "['b', 2]"),
            ("[1]", "[2, 3]"),
            ("{ a: 1 }", "{ a: 2 } as const"),
            ("{ a: 1 as const }", "{ a: 2 }"),
            ("{ a: [1] as const }", "{ a: [2 as const] }"),
        ] {
            assert_ne!(
                initializer(old, LangId::Typescript).unwrap(),
                initializer(new, LangId::Typescript).unwrap(),
                "{old} -> {new}"
            );
        }
    }

    #[test]
    fn static_shapes_require_untyped_unique_const_and_parseable_files() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for source in [
                "export let VALUE = { a: 1 };",
                "export var VALUE = { a: 1 };",
                "export const VALUE = { a: 1 }, other = 2;",
                "export const { VALUE } = { VALUE: { a: 1 } };",
                "export const VALUE = { a: 1 }; const VALUE = { a: 2 };",
                "export const VALUE = { a: 1 }; function broken( {",
                "/** @type {Shape} */ export const VALUE = { a: 1 };",
                "// ordinary\n/** @type {Shape} */\n// more\nexport const VALUE = { a: 1 };",
                "export /** @type {Shape} */ const VALUE = { a: 1 };",
                "export const VALUE = { a: /** @type {const} */ (1) };",
            ] {
                assert!(contract(source, lang).is_none(), "{lang:?}: {source}");
            }
        }
        assert!(contract("export const VALUE: Shape = { a: 1 };", LangId::Typescript).is_none());
        assert!(
            contract(
                "export const VALUE = { a: 1 }; type VALUE = number;",
                LangId::Typescript
            )
            .is_none()
        );
        let old = contract("export const VALUE = { a: 1 };", LangId::Typescript).unwrap();
        assert_ne!(
            Some(old),
            contract(
                "const VALUE = { a: 2 }; export { VALUE };",
                LangId::Typescript
            )
        );
    }

    #[test]
    fn static_shapes_bound_depth_and_total_ast_nodes() {
        let nested = format!(
            "{}1{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(initializer(&nested, LangId::Typescript).is_none());
        let large = format!("[{}]", "1,".repeat(MAX_NODES));
        assert!(initializer(&large, LangId::Typescript).is_none());
    }

    #[test]
    fn static_shape_node_kinds_exist_in_supported_grammars() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            for kind in [
                "parenthesized_expression",
                "number",
                "unary_expression",
                "string",
                "true",
                "false",
                "null",
                "object",
                "array",
                "pair",
                "property_identifier",
                "string_fragment",
                "comment",
            ] {
                tree_sitter::Query::new(&grammar, &format!("({kind})")).unwrap();
            }
            if lang != LangId::Javascript {
                tree_sitter::Query::new(&grammar, "(as_expression)").unwrap();
                tree_sitter::Query::new(&grammar, "(as_expression \"const\")").unwrap();
            }
        }
    }
}
