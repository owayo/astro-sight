//! Rust マクロ呼び出しの末尾名を AST から取り出す。

use tree_sitter::Node;

pub(crate) fn last_macro_identifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = node.child_by_field_name("macro")?;
    let mut last = None;
    let mut pending = vec![name];
    while let Some(part) = pending.pop() {
        if part.kind() == "identifier" {
            let text = part.utf8_text(source).ok()?;
            if last.is_none_or(|(start, _)| part.start_byte() > start) {
                last = Some((part.start_byte(), text));
            }
        }
        let mut cursor = part.walk();
        pending.extend(part.named_children(&mut cursor));
    }
    last.map(|(_, text)| text.strip_prefix("r#").unwrap_or(text))
}

pub(crate) fn has_token_tree_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "token_tree" {
            return true;
        }
        node = parent;
    }
    false
}

/// 構文木の一番外側の token tree の範囲 (開始の昇順で重ならない)。参照ごとに `Node::parent` で
/// 祖先をたどらずに ([`has_token_tree_ancestor`] と同じ判定を) 引く。
pub(crate) struct TokenTreeSpans(Vec<(usize, usize)>);

impl TokenTreeSpans {
    pub(crate) fn build(root: Node<'_>) -> Self {
        use crate::engine::tree_walk::{Children, Walk, walk_preorder};
        use std::ops::ControlFlow;
        let mut spans = Vec::new();
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
            if node.kind() == "token_tree" {
                spans.push((node.start_byte(), node.end_byte()));
                return ControlFlow::Continue(Walk::Skip);
            }
            ControlFlow::Continue(Walk::Descend)
        });
        Self(spans)
    }

    /// 識別子などの葉の `node` の祖先に token tree があるか。
    pub(crate) fn has_ancestor(&self, node: Node<'_>) -> bool {
        let (start, end) = (node.start_byte(), node.end_byte());
        let after = self
            .0
            .partition_point(|&(span_start, _)| span_start <= start);
        after.checked_sub(1).is_some_and(|last| {
            let (span_start, span_end) = self.0[last];
            // 一番外側の token tree そのものは、自分の祖先ではない。
            end <= span_end
                && !(node.kind() == "token_tree" && (span_start, span_end) == (start, end))
        })
    }
}

/// token tree 内の `value.include` はフィールド参照。`value.include!` は
/// マクロ展開で `value.` を捨て得るので、動的ソースの障壁として残す。
pub(crate) fn is_field_name_in_token_tree(node: Node<'_>, source: &[u8]) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(part) = previous {
        if matches!(part.kind(), "block_comment" | "line_comment") {
            previous = part.prev_sibling();
            continue;
        }
        if part.utf8_text(source).ok() != Some(".") {
            return false;
        }
        let mut next = node.next_sibling();
        while let Some(part) = next {
            if matches!(part.kind(), "block_comment" | "line_comment") {
                next = part.next_sibling();
                continue;
            }
            return part.utf8_text(source).ok().is_some_and(|text| text != "!");
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use crate::language::LangId;
    use std::ops::ControlFlow;

    /// 索引の判定は、祖先をたどる判定と識別子ごとに一致する。
    #[test]
    fn token_tree_spans_match_the_ancestor_walk() {
        let source = "#[derive(Debug)]\nstruct A { a: u8 }\nmacro_rules! m { ($x:ident) => { let $x = vec![1, (2, [3])]; }; }\nfn f(a: A) -> u8 {\n    println!(\"{}\", a.a);\n    let v = vec![a.a, inner!(b [c] {d})];\n    m!(z);\n    a.a\n}\nconst C: [u8; 2] = [1, 2];\n";
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error());
        let spans = TokenTreeSpans::build(root);
        let (mut inside, mut outside) = (0, 0);
        let _: ControlFlow<()> = walk_preorder(root, Children::All, |node, _| {
            if matches!(
                node.kind(),
                "identifier" | "field_identifier" | "type_identifier"
            ) {
                let expected = has_token_tree_ancestor(node);
                assert_eq!(
                    spans.has_ancestor(node),
                    expected,
                    "{:?}",
                    node.utf8_text(source.as_bytes())
                );
                if expected {
                    inside += 1;
                } else {
                    outside += 1;
                }
            }
            ControlFlow::Continue(Walk::Descend)
        });
        assert!(inside > 5 && outside > 5, "{inside} {outside}");
    }
}
