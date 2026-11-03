//! JSX の呼び出し位置と、関数コンポーネントの戻り値の形を、impact と API 差分で共有する。

use std::collections::HashSet;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

/// JSX 要素 1 つが props として渡すもの。AST の寿命を越えて保持するのは属性名と children の
/// 有無だけ。
pub(crate) struct JsxShape {
    /// 明示した属性名。
    pub(crate) supplied: HashSet<String>,
    /// 開始タグと終了タグの間に何か (空白を含む) があるか。
    pub(crate) children: bool,
}

impl JsxShape {
    /// `keys` のどれも渡していないか。`children` は終了タグまでの中身でも渡る。
    pub(crate) fn omits(&self, keys: &HashSet<String>) -> bool {
        keys.is_disjoint(&self.supplied) && !(keys.contains("children") && self.children)
    }
}

fn plain_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    (!text.is_empty() && !text.contains('\\')).then(|| text.to_owned())
}

/// `node` から木の根まで辿り、構文エラーが無いか。
pub(crate) fn parse_is_clean(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    !node.has_error()
}

/// タグ名の識別子 `node` から、その JSX 要素が渡す属性名と children の有無を読む。
/// spread 属性・名前空間付きの属性名・member 式のタグ・型引数・重複した属性・閉じタグの
/// 不一致など、渡すものを確定できない形は None。
pub(crate) fn caller_shape(node: Node<'_>, source: &[u8], parse_clean: bool) -> Option<JsxShape> {
    if !parse_clean {
        return None;
    }
    let open = jsx_name_opening_tag(node, source)?;
    let tag = open.child_by_field_name("name")?;
    if tag.kind() != "identifier" || open.child_by_field_name("type_arguments").is_some() {
        return None;
    }
    plain_name(tag, source)?;
    let mut supplied = HashSet::new();
    let mut cursor = open.walk();
    for attribute in open
        .named_children(&mut cursor)
        .filter(|child| child.id() != tag.id() && !child.is_extra())
    {
        if attribute.kind() != "jsx_attribute" {
            return None;
        }
        let key = attribute.named_child(0)?;
        if key.kind() != "property_identifier" || !supplied.insert(plain_name(key, source)?) {
            return None;
        }
    }
    let children = if open.kind() == "jsx_opening_element" {
        let element = open.parent()?;
        if element.kind() != "jsx_element" {
            return None;
        }
        let close = element.child_by_field_name("close_tag")?;
        let close_name = close.child_by_field_name("name")?;
        if close.kind() != "jsx_closing_element"
            || close_name.kind() != "identifier"
            || close_name.utf8_text(source).ok()? != tag.utf8_text(source).ok()?
        {
            return None;
        }
        open.end_byte() < close.start_byte()
    } else {
        false
    };
    Some(JsxShape { supplied, children })
}

/// 注釈の無い関数コンポーネントの戻り値を、`return` に直接書いた値の種類で分類する。
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) enum ReturnCategory {
    Jsx,
    String,
    Number,
    Boolean,
    Null,
}

/// `@jsx` / `@jsxImportSource` / `@jsxFrag` などの pragma コメントがあるか。TypeScript は pragma の
/// 名前の大文字小文字を区別しない (`@JSX` も効く)。
pub(crate) fn has_jsx_pragma(root: Node<'_>, source: &[u8]) -> bool {
    walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "comment"
            && source[node.byte_range()]
                .windows(4)
                .any(|window| window.eq_ignore_ascii_case(b"@jsx"))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_break()
}

fn return_category(mut value: Node<'_>, source: &[u8]) -> Option<ReturnCategory> {
    while value.kind() == "parenthesized_expression" {
        let mut cursor = value.walk();
        let mut children = value
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra());
        value = children.next()?;
        if children.next().is_some() {
            return None;
        }
    }
    match value.kind() {
        "jsx_element" | "jsx_self_closing_element" => Some(ReturnCategory::Jsx),
        "string" => Some(ReturnCategory::String),
        "number" if !value.utf8_text(source).ok()?.ends_with('n') => Some(ReturnCategory::Number),
        "true" | "false" => Some(ReturnCategory::Boolean),
        "null" => Some(ReturnCategory::Null),
        _ => None,
    }
}

/// 関数本体の `return` を集め、戻り値の種類の集合を返す。本体の最後が `return` でない場合や、
/// 分類できない値 (変数・呼び出し・型 assertion など) を返す場合は None。
pub(crate) fn inferred_returns(body: Node<'_>, source: &[u8]) -> Option<HashSet<ReturnCategory>> {
    let mut cursor = body.walk();
    if body
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .last()?
        .kind()
        != "return_statement"
    {
        return None;
    }
    let mut result = HashSet::new();
    walk_preorder(body, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "generator_function"
                | "arrow_function"
                | "method_definition"
                | "class_declaration"
                | "abstract_class_declaration"
                | "class"
        ) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "return_statement" {
            let mut cursor = node.walk();
            let mut values = node
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let Some(category) = values
                .next()
                .and_then(|value| return_category(value, source))
            else {
                return ControlFlow::Break(());
            };
            if values.next().is_some() {
                return ControlFlow::Break(());
            }
            result.insert(category);
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
    .then_some(result)
}

/// JSX の名前位置だけを解決し、終了タグも対応する開始タグへ戻す。
pub(crate) fn jsx_name_opening_tag<'a>(
    mut node: tree_sitter::Node<'a>,
    source: &[u8],
) -> Option<tree_sitter::Node<'a>> {
    let mut parent = node.parent()?;
    while parent.kind() == "member_expression" {
        if parent.child_by_field_name("property")?.id() != node.id() {
            return None;
        }
        node = parent;
        parent = node.parent()?;
    }
    if parent.child_by_field_name("name")?.id() != node.id() {
        return None;
    }
    match parent.kind() {
        "jsx_opening_element" | "jsx_self_closing_element" => Some(parent),
        "jsx_closing_element" => {
            let element = parent.parent()?;
            if element.kind() != "jsx_element" {
                return None;
            }
            let open = element.child_by_field_name("open_tag")?;
            let open_name = open.child_by_field_name("name")?;
            (open.kind() == "jsx_opening_element"
                && open_name.utf8_text(source).ok()? == node.utf8_text(source).ok()?)
            .then_some(open)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    #[test]
    fn jsx_props_shared_name_helper_keeps_member_and_closing_positions() {
        for (source, expected) in [
            ("const view = <NS.ControlPanel />;", 1),
            ("const view = <NS.ControlPanel></NS.ControlPanel>;", 2),
            (
                "const view = <ControlPanel options={ControlPanel}></ControlPanel>;",
                2,
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
            let mut count = 0;
            let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::Named, |node, _| {
                if matches!(node.kind(), "identifier" | "property_identifier")
                    && node.utf8_text(source.as_bytes()).ok() == Some("ControlPanel")
                    && jsx_name_opening_tag(node, source.as_bytes()).is_some()
                {
                    count += 1;
                }
                ControlFlow::Continue(Walk::Descend)
            });
            assert_eq!(count, expected, "{source}");
        }
    }
}
