//! 宣言と関数契約の比較で、文字列・型リテラル・コメントの原文を保持する。

use tree_sitter::Node;

/// AST の種別と原文を保持し、リテラル内の空白を整形差として消さない。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct SigTokens(pub(super) Vec<(String, String)>);

pub(super) fn signature_tokens_in_range(
    node: Node<'_>,
    source: &[u8],
    start: usize,
    end: usize,
) -> Option<SigTokens> {
    if node.has_error() || start < node.start_byte() || end > node.end_byte() {
        return None;
    }
    source.get(start..end)?;
    let mut tokens = Vec::new();
    let mut covered = start;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.end_byte() <= start || current.start_byte() >= end {
            continue;
        }
        if current.is_error() || current.is_missing() {
            return None;
        }
        if current.child_count() == 0
            || matches!(
                current.kind(),
                "string" | "template_string" | "template_literal_type" | "regex" | "comment"
            )
            || current.kind().starts_with("jsx_")
        {
            if current.start_byte() < start || current.end_byte() > end {
                return None;
            }
            let gap = std::str::from_utf8(source.get(covered..current.start_byte())?).ok()?;
            if !gap.chars().all(char::is_whitespace) {
                return None;
            }
            covered = current.end_byte();
            tokens.push((
                current.kind().to_string(),
                current.utf8_text(source).ok()?.to_string(),
            ));
        } else {
            let mut cursor = current.walk();
            stack.extend(
                current
                    .children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    let gap = std::str::from_utf8(source.get(covered..end)?).ok()?;
    gap.chars()
        .all(char::is_whitespace)
        .then_some(SigTokens(tokens))
}

pub(super) fn node_signature_tokens(node: Node<'_>, source: &[u8]) -> Option<SigTokens> {
    signature_tokens_in_range(node, source, node.start_byte(), node.end_byte())
}

#[cfg(test)]
mod tests {
    use super::super::signature::is_const_value_only_change;
    use super::*;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn unrecognized_non_whitespace_gaps_are_not_ignored() {
        let source = "const x = 1;";
        let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        let mismatched_source = "const-x = 1;";
        assert!(node_signature_tokens(tree.root_node(), mismatched_source.as_bytes()).is_none());
    }

    #[test]
    fn binding_header_tokens_preserve_literals_visibility_and_formatting() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let old = "export const f: Fn<'a b'> = x => x;";
            assert!(is_const_value_only_change(
                old,
                "export   const f : Fn<'a b'> = (x, extra = 1) => x;",
                "variable",
                lang
            ));
            for new in [
                "export const f: Fn<'a  b'> = (x, extra = 1) => x;",
                "const f: Fn<'a b'> = (x, extra = 1) => x;",
                "export let f: Fn<'a b'> = (x, extra = 1) => x;",
            ] {
                assert!(
                    !is_const_value_only_change(old, new, "variable", lang),
                    "{new}"
                );
            }
            assert!(!is_const_value_only_change(
                "export const f: Fn<`a b${string}`> = x => x;",
                "export const f: Fn<`a  b${string}`> = (x, extra = 1) => x;",
                "variable",
                lang
            ));
        }
        assert!(is_const_value_only_change(
            "pub const VALUE: &str = \"a b\";",
            "pub const VALUE: &str = \"a  b\";",
            "constant",
            LangId::Rust
        ));
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            assert!(is_const_value_only_change(
                "export const VALUE = 1;",
                "export const VALUE = 2;",
                "variable",
                lang
            ));
        }
    }
}
