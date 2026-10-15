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
    use crate::engine::{
        parser,
        tree_walk::{Children, Walk, walk_preorder},
    };
    use crate::language::LangId;
    use std::ops::ControlFlow;

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
