//! 静的な文字列を UTF-16 の値へ復元し、escape と無関係な文字列を区別する。

use super::*;
pub(super) use crate::engine::js_string::units;

pub(super) fn string(node: Node<'_>, source: &str) -> Option<String> {
    if !matches!(
        node.kind(),
        "string" | "template_string" | "template_literal_type"
    ) {
        return None;
    }
    if any_node(node, Children::Named, |child| {
        matches!(child.kind(), "template_substitution" | "template_type")
    }) {
        return None;
    }
    let raw = text(node, source)?;
    let quote = raw.as_bytes().first().copied()?;
    if !matches!(quote, b'\'' | b'"' | b'`') || raw.as_bytes().last().copied()? != quote {
        return None;
    }
    String::from_utf16(&units(raw.get(1..raw.len().checked_sub(1)?)?)?).ok()
}

pub(super) fn identifier(raw: &str) -> Option<String> {
    String::from_utf16(&units(raw)?).ok()
}
