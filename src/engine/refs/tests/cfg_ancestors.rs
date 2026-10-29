//! 祖先列を持ち回す cfg 判定と従来の親走査の一致を確認する。
use super::*;
use crate::engine::refs::definition::rust::is_rust_cfg_condition_identifier;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[test]
fn cfg_ancestor_path_matches_parent_walk() {
    let sources = [
        r##"#[cfg_attr(all(feature = "外側", not(test)), derive(Feature))]
#[cfg_attr(feature = "outer", cfg_attr(feature = "inner", cfg(feature = "deep")))]
struct Item;
fn run() {
    assert!(cfg!(all(feature = "extra", any(test, unix))));
    ordinary!{cfg!(feature = "nested"); feature; cfg_attr(feature, Other)};
    cfg_attr!(feature, 0);
    feature();
}
"##,
        "#[cfg_attr(feature = \"x\", cfg_attr(feature, cfg(feature)))]\nstruct Broken {\n",
        "fn f() { wrapper! { cfg!(all(feature, not(test))); } }\r\n",
        "#[cfg_attr(feature)] struct MissingComma;\nuse crate::feature::item;\n",
    ];
    for source in sources {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let _: std::ops::ControlFlow<()> =
            walk_preorder(tree.root_node(), Children::All, |node, _| {
                let mut ancestors = Vec::new();
                let mut parent = node.parent();
                while let Some(n) = parent {
                    ancestors.push(n);
                    parent = n.parent();
                }
                ancestors.reverse();
                assert_eq!(
                    is_rust_cfg_condition_identifier(node, &ancestors, source.as_bytes()),
                    legacy_is_rust_cfg_condition_identifier(node, source.as_bytes()),
                    "{} {:?}: {source}",
                    node.kind(),
                    node.range()
                );
                std::ops::ControlFlow::Continue(Walk::Descend)
            });
    }
}

#[test]
fn cfg_subtree_walk_keeps_outer_conditions_and_sibling_scopes() {
    let source = r#"pub fn feature() {}
#[cfg_attr(feature = "outer", cfg_attr(feature = "inner", cfg(feature = "deep")), derive(Feature))]
struct Item;
fn run() {
    wrapper!{ cfg!(feature = "nested"); ordinary!(feature); }
    feature();
}
"#;
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let kinds = definition_node_kinds(LangId::Rust);
    let full = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "feature",
        "test.rs",
        kinds,
        LangId::Rust,
    );
    assert_eq!(full.len(), 3, "定義と通常のマクロ引数と呼び出しだけ");
    let _: std::ops::ControlFlow<()> = walk_preorder(tree.root_node(), Children::All, |node, _| {
        let start = node.start_position();
        let end = node.end_position();
        let expected: Vec<_> = full
            .iter()
            .filter(|r| {
                (start.row, start.column) <= (r.line, r.column)
                    && (r.line, r.column) < (end.row, end.column)
            })
            .cloned()
            .collect();
        let actual = collect_single_refs_for_test(
            node,
            source.as_bytes(),
            "feature",
            "test.rs",
            kinds,
            LangId::Rust,
        );
        assert_eq!(
            ref_fingerprints(&actual),
            ref_fingerprints(&expected),
            "部分木 {} {:?}",
            node.kind(),
            node.range()
        );
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
}

// 比較用の旧実装。正規入力・解析回復・部分木で同じ判定を保つことを固定する。
/// `cfg_attr` の引数木かを判定する。入れ子の `cfg_attr` も許す。
fn legacy_is_cfg_attr_arguments(mut tree: Node<'_>, source: &[u8]) -> bool {
    loop {
        let Some(parent) = tree.parent() else {
            return false;
        };
        if parent.kind() == "attribute" {
            return parent
                .named_child(0)
                .and_then(|name| name.utf8_text(source).ok())
                == Some("cfg_attr");
        }
        if parent.kind() != "token_tree"
            || tree
                .prev_sibling()
                .and_then(|name| name.utf8_text(source).ok())
                != Some("cfg_attr")
        {
            return false;
        }
        tree = parent;
    }
}

/// `cfg` 条件の識別子は Rust のシンボル参照ではなく設定キーなので除外する。
///
/// tree-sitter は他のマクロの引数内にある `cfg!(...)` を `macro_invocation` にせず、
/// `identifier`・`!`・`token_tree` に分ける。引数木の直前の兄弟を見ることで
/// その形も扱う。`cfg_attr` は最初の引数だけが条件なので、直下のカンマまでに
/// 限る。後続の `derive(Trait)` は実際のシンボル参照として残す。
fn legacy_is_rust_cfg_condition_identifier(node: Node<'_>, source: &[u8]) -> bool {
    let mut ancestor = node.parent();
    while let Some(tree) = ancestor {
        ancestor = tree.parent();
        if tree.kind() != "token_tree" {
            continue;
        }
        let Some(previous) = tree.prev_sibling() else {
            continue;
        };
        let (name_node, is_macro) = if previous.kind() == "!" {
            (previous.prev_sibling(), true)
        } else {
            (Some(previous), false)
        };
        let Some(name) = name_node.and_then(|n| n.utf8_text(source).ok()) else {
            continue;
        };
        let is_attribute = tree.parent().is_some_and(|parent| {
            parent.kind() == "attribute"
                || (parent.kind() == "token_tree" && legacy_is_cfg_attr_arguments(parent, source))
        });
        if !is_macro && !is_attribute {
            continue;
        }
        match name {
            "cfg" => return true,
            "cfg_attr" if !is_macro => {
                let mut cursor = tree.walk();
                return tree
                    .children(&mut cursor)
                    .find(|child| child.kind() == ",")
                    .is_some_and(|comma| node.end_byte() <= comma.start_byte());
            }
            _ => {}
        }
    }
    false
}
