//! 型注釈のない const の初期化子を元ソースで比較する。

use std::collections::{BTreeSet, HashSet};

use tree_sitter::Node;

use super::js_const::unique_top_level_const;
use super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::language::LangId;

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ScalarKind {
    Number,
    BigInt,
    String,
    Boolean,
    Null,
    /// `undefined`。container の葉としては扱わず、トップレベルの scalar 比較だけで使う。
    Undefined,
}

/// 型注釈のない JS/TS の scalar literal の種類。推論される公開型の種類が同じかを比べるために
/// 使う (`30` → `"30s"` は number から string への変更)。scalar literal でなければ None。
pub(super) fn js_scalar_literal_kind(node: Node<'_>, source: &[u8]) -> Option<ScalarKind> {
    match node.kind() {
        "number" => number_kind(node, source),
        "string" => Some(ScalarKind::String),
        "true" | "false" => Some(ScalarKind::Boolean),
        "null" => Some(ScalarKind::Null),
        "undefined" => Some(ScalarKind::Undefined),
        _ => None,
    }
}

/// 同種のリテラルだけからなる数値の算術式か文字列の連結を証明する。値は評価しない。
pub(super) fn js_static_binary_kind(value: Node<'_>, source: &[u8]) -> Option<ScalarKind> {
    if value.has_error() || value.is_missing() {
        return None;
    }
    let mut stack = vec![value];
    let mut kind = None;
    let mut has_binary_operator = false;
    let mut needs_number = false;
    while let Some(node) = stack.pop() {
        let leaf_kind = match node.kind() {
            "number" if number_kind(node, source)? == ScalarKind::Number => ScalarKind::Number,
            "string" => ScalarKind::String,
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let mut children = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment");
                stack.push(children.next()?);
                if children.next().is_some() {
                    return None;
                }
                continue;
            }
            "unary_expression" if node.child_by_field_name("operator")?.kind() == "-" => {
                needs_number = true;
                stack.push(node.child_by_field_name("argument")?);
                continue;
            }
            "binary_expression" => {
                match node.child_by_field_name("operator")?.kind() {
                    "+" => {}
                    "-" | "*" | "/" | "%" | "**" => needs_number = true,
                    _ => return None,
                }
                has_binary_operator = true;
                stack.push(node.child_by_field_name("left")?);
                stack.push(node.child_by_field_name("right")?);
                continue;
            }
            _ => return None,
        };
        if *kind.get_or_insert(leaf_kind) != leaf_kind {
            return None;
        }
    }
    kind.filter(|kind| has_binary_operator && (!needs_number || *kind == ScalarKind::Number))
}

#[derive(Debug, PartialEq, Eq)]
enum StaticShape {
    Scalar(ScalarKind),
    /// `as const` の内側でも演算結果は literal 型にならない。
    Expression(ScalarKind),
    /// 直に代入した TS テンプレートの置換式。式の順序と原文を保つ。
    InterpolatedTemplate(Vec<String>),
    Object(Vec<(String, StaticShape)>),
    Array(Vec<StaticShape>),
    ConstAssert(Box<StaticShape>),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WidenedShape {
    Scalar(ScalarKind),
    Object(Vec<(String, WidenedShape)>),
    Array(Vec<WidenedShape>),
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
    // container も ${...} もない候補をソースの読み込み前に除く。
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
    if !same_contract(&old, &new) {
        return StaticConstChange::NotApplicable;
    }
    if matches!(old.shape, StaticShape::InterpolatedTemplate(_)) {
        let old_range = old_value.byte_range();
        let new_range = new_value.byte_range();
        let before = &source.old[old_range.clone()];
        let after = &source.new[new_range.clone()];
        // stripInternal の加工前も同じ初期化子で、固定文字が実際に変わった場合だけを扱う。
        return if before != after
            && source.original_old().get(old_range) == Some(before)
            && source.original_new().get(new_range) == Some(after)
            && site.old_path == site.new_path
        {
            StaticConstChange::ValueOnly
        } else {
            StaticConstChange::NotApplicable
        };
    }
    let (Some(old_tokens), Some(new_tokens)) = (
        primitive_value_tokens(old_value, &source.old),
        primitive_value_tokens(new_value, &source.new),
    ) else {
        return StaticConstChange::NotApplicable;
    };
    if old.shape == new.shape && old_tokens == new_tokens {
        StaticConstChange::Unchanged
    } else {
        StaticConstChange::ValueOnly
    }
}

fn same_contract(old: &StaticConstContract, new: &StaticConstContract) -> bool {
    if old.prefix != new.prefix || old.suffix != new.suffix {
        return false;
    }
    if old.shape == new.shape {
        return true;
    }
    matches!(
        (widened_shape(&old.shape), widened_shape(&new.shape)),
        (Some(old), Some(new)) if old == new
    )
}

fn widened_shape(shape: &StaticShape) -> Option<WidenedShape> {
    match shape {
        StaticShape::Scalar(kind) | StaticShape::Expression(kind) => {
            Some(WidenedShape::Scalar(*kind))
        }
        StaticShape::Object(properties) => Some(WidenedShape::Object(
            properties
                .iter()
                .map(|(key, value)| Some((key.clone(), widened_shape(value)?)))
                .collect::<Option<Vec<_>>>()?,
        )),
        StaticShape::Array(elements) => Some(WidenedShape::Array(
            elements
                .iter()
                .map(widened_shape)
                .collect::<Option<BTreeSet<_>>>()?
                .into_iter()
                .collect(),
        )),
        // Literal types and readonly tuples cannot be reduced to a set of widened shapes.
        StaticShape::ConstAssert(_) | StaticShape::InterpolatedTemplate(_) => None,
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
    let shape =
        if value.kind() == "template_string" && matches!(lang, LangId::Typescript | LangId::Tsx) {
            let mut substitutions = Vec::new();
            for child in value.named_children(&mut value.walk()) {
                match child.kind() {
                    "template_substitution" => {
                        substitutions.push(child.utf8_text(source).ok()?.to_owned())
                    }
                    _ if super::js_const::is_template_text(child, source) => {}
                    _ => return None,
                }
            }
            if substitutions.is_empty() {
                return None;
            }
            StaticShape::InterpolatedTemplate(substitutions)
        } else {
            let shape = static_shape(value, source, lang, 0)?;
            is_container(&shape).then_some(shape)?
        };
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

#[derive(Debug, PartialEq, Eq)]
enum ValueToken {
    Literal(String),
    Expression(SigTokens),
}

fn primitive_value_tokens(node: Node<'_>, source: &[u8]) -> Option<Vec<ValueToken>> {
    let mut tokens = Vec::new();
    collect_primitive_value_tokens(node, source, &mut tokens)?;
    Some(tokens)
}

// 構造一致を証明した初期化子だけを走査し、キー・括弧・カンマ・コメントを含めない。
fn collect_primitive_value_tokens(
    node: Node<'_>,
    source: &[u8],
    tokens: &mut Vec<ValueToken>,
) -> Option<()> {
    match node.kind() {
        "number" | "true" | "false" | "null" => {
            tokens.push(ValueToken::Literal(node.utf8_text(source).ok()?.to_owned()));
        }
        "string" => {
            tokens.push(ValueToken::Literal(
                plain_string_value(node.utf8_text(source).ok()?).to_owned(),
            ));
        }
        "unary_expression" if node.child_by_field_name("argument")?.kind() == "number" => {
            let argument = node
                .child_by_field_name("argument")?
                .utf8_text(source)
                .ok()?;
            tokens.push(ValueToken::Literal(format!("-{argument}")));
        }
        "binary_expression" | "unary_expression" => {
            let mut expression = header_tokens(node, source, node.start_byte(), node.end_byte())?;
            for (kind, text) in &mut expression.0 {
                if kind == "string" {
                    *text = plain_string_value(text).to_owned();
                }
            }
            tokens.push(ValueToken::Expression(expression));
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

/// escape のない通常文字列だけ、引用符の違いを整形として除く。
fn plain_string_value(text: &str) -> &str {
    if text.contains('\\') {
        return text;
    }
    text.strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .or_else(|| {
            text.strip_prefix('"')
                .and_then(|text| text.strip_suffix('"'))
        })
        .unwrap_or(text)
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
        StaticShape::Scalar(_)
        | StaticShape::Expression(_)
        | StaticShape::InterpolatedTemplate(_) => false,
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
        "unary_expression"
            if node.child_by_field_name("operator")?.kind() == "-"
                && node.child_by_field_name("argument")?.kind() == "number" =>
        {
            number_shape(node.child_by_field_name("argument")?, source)
        }
        "binary_expression" | "unary_expression" => {
            js_static_binary_kind(node, source).map(StaticShape::Expression)
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
            let shape = static_shape(expression, source, lang, depth + 1)?;
            (!matches!(shape, StaticShape::Expression(_)))
                .then_some(StaticShape::ConstAssert(Box::new(shape)))
        }
        _ => None,
    }
}

fn number_shape(node: Node<'_>, source: &[u8]) -> Option<StaticShape> {
    Some(StaticShape::Scalar(number_kind(node, source)?))
}

fn number_kind(node: Node<'_>, source: &[u8]) -> Option<ScalarKind> {
    Some(if node.utf8_text(source).ok()?.ends_with('n') {
        ScalarKind::BigInt
    } else {
        ScalarKind::Number
    })
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
    fn template_contract_keeps_substitution_order_and_original_literal_text() {
        let template = "`a${first}${format('two words')}`";
        let old = initializer(template, LangId::Typescript).unwrap();
        let new = initializer("`b${first}c${format('two words')}d`", LangId::Typescript).unwrap();
        assert!(same_contract(&old, &new));
        for changed in [
            "`b${format('two words')}${first}`",
            "`b${first}${format('two  words')}`",
            "`b${first}`",
        ] {
            let new = initializer(changed, LangId::Typescript).unwrap();
            assert!(!same_contract(&old, &new), "{changed}");
        }
        for unsupported in [
            "'fixed'",
            "`fixed`",
            "String.raw`b${first}`",
            "`b${first}` as const",
            "`b${first}` satisfies string",
        ] {
            assert!(
                initializer(unsupported, LangId::Typescript).is_none(),
                "{unsupported}"
            );
        }
        assert!(initializer(template, LangId::Javascript).is_none());
    }

    #[test]
    fn untyped_arrays_compare_distinct_widened_element_shapes() {
        for (old, new) in [
            ("['s', 'm', 'l']", "['s', 'm']"),
            ("[1]", "[2, 3]"),
            ("[1, 'a', 2]", "['b', 3, 4, 'c']"),
            ("{ sizes: ['s', 'm'] }", "{ sizes: ['l'] }"),
            ("[[1, 2], ['a']]", "[['b'], [3]]"),
            ("[[1], [2]]", "[[1, 2]]"),
        ] {
            let old = initializer(old, LangId::Typescript).unwrap();
            let new = initializer(new, LangId::Typescript).unwrap();
            assert!(same_contract(&old, &new), "{old:?} -> {new:?}");
        }
        for (old, new) in [
            ("[1, 'a']", "[2]"),
            ("[]", "[1]"),
            ("[1]", "[]"),
            ("[1, 2] as const", "[3] as const"),
            ("[1 as const, 2 as const]", "[3 as const]"),
            ("{ sizes: [1, 2] } as const", "{ sizes: [3] } as const"),
        ] {
            let old = initializer(old, LangId::Typescript).unwrap();
            let new = initializer(new, LangId::Typescript).unwrap();
            assert!(!same_contract(&old, &new), "{old:?} -> {new:?}");
        }
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
            assert_eq!(
                values(original),
                ["-1", "a", "true", "null", "2n"]
                    .map(|value| ValueToken::Literal(value.to_owned()))
            );
            assert_eq!(values(original), values(formatted), "{lang:?}");
            assert_eq!(values("['a']"), values("[\"a\"]"), "{lang:?}");
            assert_eq!(
                values("['a'+'b']"),
                values("[\"a\" /* comment */ + 'b']"),
                "{lang:?}"
            );
            for (old, new) in [
                ("[0x10]", "[16]"),
                ("['a']", "['b']"),
                ("['a\\n']", "[\"a\\n\"]"),
                ("[-1]", "[1]"),
                ("[1n]", "[2n]"),
                ("[true]", "[false]"),
                ("['a b']", "['a  b']"),
                ("[2 * 3]", "[2 + 3]"),
                ("[(1 + 2) * 3]", "[1 + 2 * 3]"),
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
                "{ a: '1' + 2 }",
                "{ a: BASE * 2 }",
                "{ a: String(1) + 'x' }",
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
                "{ a: (1 + 2) as const }",
            ] {
                assert!(initializer(value, lang).is_none(), "{value}");
            }
        }
        assert!(initializer("<const>{ a: 1 }", LangId::Typescript).is_none());
    }

    #[test]
    fn expression_results_widen_like_literals_only_outside_const_assertions() {
        for (old, new, equal) in [
            ("{ a: 'x' }", "{ a: 'y' + 'z' }", true),
            ("[1 + 2]", "[2, 3]", true),
            ("{ a: 1 + 2 } as const", "{ a: -(2 * 3) } as const", true),
            ("{ a: 'x' } as const", "{ a: 'y' + 'z' } as const", false),
            ("{ a: 'x' + 'y' }", "{ a: 1 + 2 }", false),
        ] {
            let old = initializer(old, LangId::Typescript).unwrap();
            let new = initializer(new, LangId::Typescript).unwrap();
            assert_eq!(same_contract(&old, &new), equal, "{old:?} -> {new:?}");
        }
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
