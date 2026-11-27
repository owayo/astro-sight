//! Rust ソースを文書の文字列として取り込み得るマクロを調べる。

use tree_sitter::Node;

use crate::engine::rust_macro_name::last_macro_identifier;

/// 読む先を確定できればその字面、マクロの別名・動的な引数なら `None` を渡す。
/// 明らかに Rust ソースでない文書には述語を呼ばない。
pub(crate) fn any_rust_text_include<'a>(
    root: Node<'_>,
    source: &'a [u8],
    mut matches: impl FnMut(Option<&'a str>) -> bool,
) -> bool {
    any_rust_text_include_at(root, source, |_, literal| matches(literal))
}

pub(crate) fn any_rust_text_include_at<'a>(
    root: Node<'_>,
    source: &'a [u8],
    mut matches: impl FnMut(Node<'_>, Option<&'a str>) -> bool,
) -> bool {
    if memchr::memmem::find(source, b"include_str").is_none() {
        return false;
    }
    crate::engine::tree_walk::any_node(root, crate::engine::tree_walk::Children::All, |node| {
        if node.kind() == "identifier"
            && node
                .utf8_text(source)
                .is_ok_and(|name| name.strip_prefix("r#").unwrap_or(name) == "include_str")
            && !followed_by_bang(node)
            && binds_macro_names(node)
        {
            return matches(node, None);
        }
        let Some(arguments) = text_include_arguments(node, source) else {
            return false;
        };
        let mut cursor = arguments.walk();
        let mut named = arguments
            .named_children(&mut cursor)
            .filter(|n| !n.is_extra());
        let Some(literal) = named
            .next()
            .filter(|node| matches!(node.kind(), "string_literal" | "raw_string_literal"))
        else {
            return matches(node, None);
        };
        let Some(path) = literal
            .named_child(0)
            .filter(|n| {
                named.next().is_none()
                    && literal.named_child_count() == 1
                    && n.kind() == "string_content"
            })
            .and_then(|n| n.utf8_text(source).ok())
            .filter(|text| !text.contains('\\'))
        else {
            return matches(node, None);
        };
        literal_may_open_rust_source(path) && matches(node, Some(path))
    })
}

/// 識別子 `node` (パスの最後の要素なら、そのパス) の次の字句が `!` か。
fn followed_by_bang(node: tree_sitter::Node<'_>) -> bool {
    let mut current = node;
    loop {
        // 兄弟の API は名前が合ったときだけ呼ぶ (根から探し直すので重い)。
        let mut next = current.next_sibling();
        while let Some(sibling) = next.filter(|sibling| sibling.is_extra()) {
            next = sibling.next_sibling();
        }
        match next {
            Some(next) => return next.kind() == "!",
            None => match current.parent() {
                Some(parent) if parent.kind() == "scoped_identifier" => current = parent,
                _ => return false,
            },
        }
    }
}

/// 識別子 `node` が、マクロの名前を束縛し得る位置 (`use` の宣言・`extern crate`・マクロの引数の
/// token tree の中) にあるか。普通の式や宣言の識別子 (`let include_str = false;`) はマクロの名前を
/// 束縛しない。親の API は根から探し直すので、名前が合ったときだけ呼ぶ。
fn binds_macro_names(node: tree_sitter::Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "use_declaration" | "extern_crate_declaration" | "token_tree"
        ) {
            return true;
        }
        current = parent;
    }
    false
}

/// `include_str!(..)` の呼び出しの引数の token tree。
fn text_include_arguments<'t>(
    node: tree_sitter::Node<'t>,
    source: &[u8],
) -> Option<tree_sitter::Node<'t>> {
    let text_include = |name: &str| name == "include_str";
    match node.kind() {
        "macro_invocation" => {
            if !text_include(last_macro_identifier(node, source)?) {
                return None;
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "token_tree")
        }
        "identifier" => {
            let name = node.utf8_text(source).ok()?;
            if !text_include(name.strip_prefix("r#").unwrap_or(name)) {
                return None;
            }
            let mut next = node.next_sibling()?;
            while next.is_extra() {
                next = next.next_sibling()?;
            }
            if next.kind() != "!" {
                return None;
            }
            let mut arguments = next.next_sibling()?;
            while arguments.is_extra() {
                arguments = arguments.next_sibling()?;
            }
            (arguments.kind() == "token_tree").then_some(arguments)
        }
        _ => None,
    }
}

/// `include_str!` の字面 1 つ (`"../README.md"`。escape は呼び出し側で除く) が `.rs` を開き得るか。
/// Windows の開き方も見る: 要素の末尾の `.`・空白を落とし、`.` の要素を畳むので、`lib.rs.`・
/// `lib.rs/.`・`lib.rs ` も `lib.rs` を読む。`:` (`lib.rs::$DATA` のストリームの名前) と、名前の
/// 要素より後ろに親の要素 (`lib.rs/unused/../.`) があるものは決めない。
fn literal_may_open_rust_source(path: &str) -> bool {
    if path.contains(':') {
        return true;
    }
    let mut named = false;
    let mut last = None;
    for component in path.split('/') {
        // `.`・空白だけの要素: `.` と空は飛ばし、ほか (`..`・`...`・`.. `) は親を指し得る。名前の
        // 要素より後ろの親 (`lib.rs/unused/../.`) は、Windows が畳むので最後の要素を決めない。
        if component.chars().all(|c| matches!(c, '.' | ' ')) {
            if matches!(component, "" | ".") {
                continue;
            }
            if named {
                return true;
            }
            continue;
        }
        named = true;
        last = Some(component.trim_end_matches(['.', ' ']));
    }
    last.is_none_or(|component| {
        component
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("rs"))
    })
}
