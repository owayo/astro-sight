//! Rust の取り込み構文を、空白とコメントに依存せず検出する。

mod text_include;
pub(crate) use text_include::any_rust_text_include;

use tree_sitter::Node;

use super::rust_macro_name::{
    has_token_tree_ancestor, is_field_name_in_token_tree, last_macro_identifier,
};

fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

fn previous_token(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.prev_sibling()?;
        if !is_comment(node) {
            return Some(node);
        }
    }
}

fn named(node: Node<'_>, source: &[u8], name: &str) -> bool {
    node.kind() == "identifier"
        && node
            .utf8_text(source)
            .ok()
            .is_some_and(|text| text.strip_prefix("r#").unwrap_or(text) == name)
}

fn contains_name(root: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if named(node, source, name) {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn contains_kind(root: Node<'_>, kind: &str) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == kind {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// `#[path]` を選び得る属性。マクロの定義の中で名前の位置がメタ変数か繰り返しの属性 (`#[$attr]`・
/// `#[$($t)+]`) と、
/// メタ変数を渡す `cfg_attr` (`#[cfg_attr(all(), $attr)]`) は、展開で `path` になり得る。
fn attribute_selects_path(attribute: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attribute.walk();
    let Some(name) = attribute
        .named_children(&mut cursor)
        .find(|child| !is_comment(*child))
    else {
        return false;
    };
    named(name, source, "path")
        || matches!(name.kind(), "metavariable" | "token_repetition")
        || named(name, source, "cfg_attr")
            && (contains_name(attribute, source, "path")
                || contains_kind(attribute, "metavariable"))
}

fn is_attribute_token_tree(node: Node<'_>, source: &[u8]) -> bool {
    if node.child(0).and_then(|child| child.utf8_text(source).ok()) != Some("[") {
        return false;
    }
    let Some(previous) = previous_token(node) else {
        return false;
    };
    match previous.utf8_text(source).ok() {
        Some("#") => true,
        Some("!") => {
            previous_token(previous).and_then(|token| token.utf8_text(source).ok()) == Some("#")
        }
        _ => false,
    }
}

/// 角括弧の token tree で、中の最初のトークンが `path` か、`path`・メタ変数を含む `cfg_attr`
/// (`mounted!([path = "..."])`)。最初がメタ変数の角括弧 (`[$x]`) は配列の式なので数えない
/// (属性になる `# $attr` は [`hash_then_metavariable`] が見る)。
fn bracketed_path(node: Node<'_>, source: &[u8]) -> bool {
    if node.child(0).and_then(|child| child.utf8_text(source).ok()) != Some("[") {
        return false;
    }
    let mut cursor = node.walk();
    let Some(name) = node
        .named_children(&mut cursor)
        .find(|child| !is_comment(*child))
    else {
        return false;
    };
    named(name, source, "path")
        || named(name, source, "cfg_attr")
            && (contains_name(node, source, "path") || contains_kind(node, "metavariable"))
}

/// token tree の中で、`#` の直後 (コメントを除く) がメタ変数か繰り返し。
/// token tree の中で、角括弧の token tree (最初のトークンがメタ変数・繰り返し・`path`・`cfg_attr`)
/// の後ろに、属性・可視性・メタ変数を挟んで `mod` が続くか (`$h [$p = "..."] mod copied;` は
/// `$h` に `#`、`$p` に `path` を渡すと `#[path = "..."]` になる)。配列や添字の式 (`[$x]`・
/// `$v[$i]`) の後ろに `mod` は続かない。
fn bracket_before_mod(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    let tokens: Vec<_> = node
        .children(&mut cursor)
        .filter(|child| !is_comment(*child))
        .collect();
    let text = |token: Node<'_>| token.utf8_text(source).unwrap_or_default();
    let bracket = |token: Node<'_>| {
        token.kind() == "token_tree" && token.child(0).is_some_and(|open| text(open) == "[")
    };
    tokens.iter().enumerate().any(|(index, &token)| {
        if !bracket(token) {
            return false;
        }
        let mut inner = token.walk();
        let Some(first) = token
            .named_children(&mut inner)
            .find(|child| !is_comment(*child))
        else {
            return false;
        };
        if !(matches!(first.kind(), "metavariable" | "token_repetition")
            || named(first, source, "path")
            || named(first, source, "cfg_attr"))
        {
            return false;
        }
        for &next in &tokens[index + 1..] {
            match next.kind() {
                // メタ変数は `mod` (や可視性) を渡され得る。
                "metavariable" | "token_repetition" => return true,
                "#" => {}
                "token_tree" if matches!(text(next).chars().next(), Some('[' | '(')) => {}
                _ if matches!(text(next), "pub" | "unsafe") => {}
                _ => return text(next) == "mod",
            }
        }
        false
    })
}

fn hash_then_metavariable(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let tokens: Vec<_> = node
        .children(&mut cursor)
        .filter(|child| !is_comment(*child))
        .collect();
    tokens.windows(2).any(|pair| {
        pair[0].kind() == "#" && matches!(pair[1].kind(), "metavariable" | "token_repetition")
    })
}

/// 既存の AST 走査から呼び出せる、ノード単位の動的ソース障壁。
pub(crate) fn node_includes_external_source(node: Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "attribute" => attribute_selects_path(node, source),
        "token_tree" if is_attribute_token_tree(node, source) => {
            attribute_selects_path(node, source)
        }
        // 角括弧の中が `path`・`cfg_attr` で始まる (`mounted!([path = "..."])` は、マクロの中の
        // `# $attr` で属性になり得る)。マクロの定義の中の `#` の直後のメタ変数も属性になり得る。
        // マクロの本体の繰り返し (`$( # $br mod copied; )*`) も同じ。
        "token_tree" | "token_repetition" => {
            bracketed_path(node, source)
                || hash_then_metavariable(node)
                || bracket_before_mod(node, source)
        }
        "macro_invocation" => {
            last_macro_identifier(node, source).is_none_or(|name| name == "include")
        }
        "identifier"
            if has_token_tree_ancestor(node) && !is_field_name_in_token_tree(node, source) =>
        {
            named(node, source, "include")
        }
        "use_declaration" => contains_name(node, source, "include"),
        _ => false,
    }
}

/// マクロの本体の繰り返しの中の `# $br mod copied;` も、`#[path]` に組み立て得る。
#[cfg(test)]
#[test]
fn repetition_bodies_include_external_sources() {
    let source = "macro_rules! mounted {\n    ($($br:tt),*) => { $( # $br mod copied; )* };\n}\n";
    let tree =
        crate::engine::parser::parse_source(source.as_bytes(), crate::language::LangId::Rust)
            .unwrap();
    assert!(tree_includes_external_source(
        tree.root_node(),
        source.as_bytes()
    ));
}

pub(crate) fn tree_includes_external_source(root: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node_includes_external_source(node, source) {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::tree_includes_external_source;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn external_source_syntax_ignores_spacing_and_comments() {
        for (source, expected) in [
            ("#[path = \"x.inc\"] mod x;", true),
            ("# [path = \"x.inc\"] mod x;", true),
            ("# /*a*/ [path /*b*/ = \"x.inc\"] mod x;", true),
            ("#[cfg_attr(all(), path = \"x.inc\")] mod x;", true),
            (
                "macro_rules! m { () => { # [cfg_attr(all(), path = \"x.inc\")] mod x; } }",
                true,
            ),
            (
                "macro_rules! attach { ($attr:meta) => { #[$attr] mod copied; }; }",
                true,
            ),
            (
                "macro_rules! attach { ($c:meta) => { #[cfg_attr(all(), $c)] mod copied; }; }",
                true,
            ),
            (
                "macro_rules! attach { ($($t:tt)+) => { #[$($t)+] mod copied; }; }",
                true,
            ),
            (
                "macro_rules! mounted { ($attr:tt) => { # $attr mod copied; }; }",
                true,
            ),
            ("mounted!([path = \"../b/src/calls.rs\"]);", true),
            ("macro_rules! one { ($x:expr) => { [$x] }; }", false),
            (
                "macro_rules! get { ($v:expr, $i:expr) => { $v[$i] }; }",
                false,
            ),
            (
                "macro_rules! mount { ($h:tt, $p:ident) => { $h [$p = \"x.rs\"] mod copied; }; }",
                true,
            ),
            (
                "macro_rules! documented { ($doc:expr) => { #[doc = $doc] fn f() {} }; }",
                false,
            ),
            ("include /*note*/ !(\"x.inc\");", true),
            ("use std::{include as load}; load!(\"x.inc\");", true),
            ("#[inline] fn f() {}", false),
            ("#[doc = \"path\"] fn f() {}", false),
            ("fn f() { let path = 1; let s = \"#[path = x]\"; }", false),
            ("macro_rules! m { ($p:expr) => { let path = $p; } }", false),
            ("fn f() { println!(\"{}\", args.include); }", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).expect("parse Rust");
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                tree_includes_external_source(tree.root_node(), source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }
}
