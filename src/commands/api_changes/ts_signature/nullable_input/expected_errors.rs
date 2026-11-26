//! TypeScript の期待した診断が出る行を、解析済みのコメントから特定する。

use std::{collections::HashMap, ops::Range};
use tree_sitter::Node;

use super::super::callable::unwrap_parentheses;
use super::super::removed_params::{InventoryView, checked_nodes, unique_function_declaration};
use crate::commands::api_changes::{
    static_js_string_text, ts_export_bindings::visit_value_imports,
};
use crate::engine::js_scope::{Namespace, ScopeBinding, resolve_scope_binding};
use crate::engine::js_string::decoded;

const LINE_BREAKS: [char; 4] = ['\n', '\r', '\u{2028}', '\u{2029}'];

fn whitespace(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{0085}') || ch == '\u{feff}'
}

/// 行コメントでは 2〜3 個の / の直後、ブロックコメントでは最終行の先頭を調べる。
fn directive(text: &str) -> Option<bool> {
    let body = if let Some(body) = text.strip_prefix("//") {
        body.strip_prefix('/').unwrap_or(body)
    } else {
        text.strip_prefix("/*")?;
        text.rsplit(LINE_BREAKS)
            .next()?
            .trim_start_matches(whitespace)
            .trim_start_matches(['/', '*'])
    };
    let tag = body.trim_start_matches(whitespace);
    if tag.starts_with("@ts-expect-error") {
        Some(true)
    } else if tag.starts_with("@ts-ignore") {
        Some(false)
    } else {
        None
    }
}

struct ExpectedErrorLines(Vec<Range<usize>>);

impl ExpectedErrorLines {
    fn collect(nodes: &[Node<'_>], source: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(source).ok()?;
        let mut directives: Vec<_> = nodes
            .iter()
            .filter(|node| node.kind() == "comment")
            .filter_map(|node| {
                let raw = &text[node.byte_range()];
                let expect = directive(raw)?;
                let offset = if raw.starts_with("//") {
                    node.start_byte()
                } else {
                    node.end_byte() - 1
                };
                Some((offset, expect))
            })
            .collect();
        if directives.is_empty() {
            return Some(Self(Vec::new()));
        }
        directives.sort_unstable();
        let mut directives = directives.into_iter().peekable();
        let mut lines = Vec::new();
        let mut start = 0;
        let mut expect = false;
        for raw in text.split_inclusive(LINE_BREAKS) {
            let end = start + raw.len();
            let line = raw.trim_matches(whitespace);
            // 空行と // だけの行をまたいで、直近の指示が次のコード行に掛かる。
            if !line.is_empty() && !line.starts_with("//") {
                if expect {
                    lines.push(start..end);
                }
                expect = false;
            }
            while let Some((_, next)) = directives.next_if(|&(offset, _)| offset < end) {
                expect = next;
            }
            start = end;
        }
        Some(Self(lines))
    }

    fn intersects(&self, node: Node<'_>) -> bool {
        let next = self.0.partition_point(|line| line.end <= node.start_byte());
        self.0
            .get(next)
            .is_some_and(|line| line.start < node.end_byte())
    }
}

/// 直接の呼び出し名、または対象のメンバーを読むレシーバー。動的な添字は対象を排除できない。
fn call_binding<'a>(call: Node<'a>, source: &[u8], name: &str) -> Option<(Node<'a>, bool)> {
    let callee = if call.kind() == "call_expression" {
        unwrap_parentheses(call.child_by_field_name("function")?)?
    } else {
        let open = call.child_by_field_name("open_tag").unwrap_or(call);
        let tag = open.child_by_field_name("name")?;
        if tag.kind() == "identifier" {
            let raw = tag.utf8_text(source).ok()?;
            if raw.starts_with(|c: char| c.is_ascii_lowercase()) || raw.contains('-') {
                return None;
            }
        }
        tag
    };
    if callee.kind() == "identifier" {
        return Some((callee, false));
    }
    match callee.kind() {
        "member_expression" => {
            let key = callee
                .child_by_field_name("property")?
                .utf8_text(source)
                .ok()?;
            if decoded(key)?.as_ref() != name {
                return None;
            }
        }
        "subscript_expression" => {
            if static_js_string_text(callee.child_by_field_name("index")?, source)
                .is_some_and(|key| key != name)
            {
                return None;
            }
        }
        _ => return None,
    }
    let object = unwrap_parentheses(callee.child_by_field_name("object")?)?;
    (object.kind() == "identifier").then_some((object, true))
}

/// 対象関数の宣言・呼び出しに掛かる指示は、呼び出し行も同時に編集していても残す。
pub(super) fn affects_widening(
    inventory: &InventoryView<'_>,
    target: &str,
    name: &str,
) -> Option<bool> {
    for (path, source, root, _) in inventory.parsed() {
        if memchr::memmem::find(source, b"@ts-expect-error").is_none() {
            continue;
        }
        let mut imports = HashMap::new();
        for import in root
            .named_children(&mut root.walk())
            .filter(|n| n.kind() == "import_statement")
        {
            if inventory.import_target(path, import, source).as_deref() == Some(target) {
                visit_value_imports(import, source, |local, exported| {
                    if exported.as_deref().is_none_or(|exported| exported == name) {
                        imports.insert(local, (import, exported.is_none()));
                    }
                });
            }
        }
        if path != target && imports.is_empty() {
            continue;
        }
        let nodes = checked_nodes(root)?;
        let lines = ExpectedErrorLines::collect(&nodes, source)?;
        if lines.0.is_empty() {
            continue;
        }
        if path == target && lines.intersects(unique_function_declaration(root, source, name)?.1) {
            return Some(true);
        }
        for call in nodes.iter().filter(|n| {
            matches!(
                n.kind(),
                "call_expression" | "jsx_element" | "jsx_self_closing_element"
            ) && lines.intersects(**n)
        }) {
            let Some((binding, member)) = call_binding(*call, source, name) else {
                continue;
            };
            let raw = binding.utf8_text(source).ok()?;
            let local = decoded(raw)?;
            if path == target && !member && local == name {
                return Some(true);
            }
            if let Some(&(import, namespace)) = imports.get(local.as_ref())
                && member == namespace
                && match resolve_scope_binding(binding, source, raw, Namespace::Value) {
                    Some(ScopeBinding::Import(owner)) => owner == import,
                    None => true,
                    _ => false,
                }
            {
                return Some(true);
            }
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn expected_errors_follow_typescript_directive_lines() {
        for (prefix, expected) in [
            ("/* note\n @ts-expect-error */\r\n", true),
            ("/* note\n  * @ts-expect-error */\r\n", true),
            ("// @ts-expect-error\u{2028}\u{2029}", true),
            ("// @ts-expect-error\r\n//\u{0085}@ts-ignore\r\n", true),
            ("// @ts-expect-error\n// @ts-ignore\n", false),
            ("/* @ts-expect-error\n ordinary last line */\n", false),
            ("//// @ts-expect-error\n", false),
            ("// @ts-expect-error\n/* other comment */\n", false),
        ] {
            let source = format!("{prefix}word(null);");
            let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
            let nodes = checked_nodes(tree.root_node()).unwrap();
            let call = nodes
                .iter()
                .find(|node| node.kind() == "call_expression")
                .unwrap();
            let lines = ExpectedErrorLines::collect(&nodes, source.as_bytes()).unwrap();
            assert_eq!(lines.intersects(*call), expected, "{source:?}");
        }
    }
}
