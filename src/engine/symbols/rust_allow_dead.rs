//! Rust の関数に付けた明示的な dead-code 抑制。参照や API 面の証明には使わない。

use tree_sitter::Node;

use crate::models::location::Range;

/// 関数宣言の直前の独立した行コメントだけを意思表示として受け付ける。
pub(crate) fn rust_has_allow_dead_marker(root: Node<'_>, source: &[u8], range: &Range) -> bool {
    let Some(decl) = super::node_for_symbol_range(root, range) else {
        return false;
    };
    if !matches!(decl.kind(), "function_item" | "function_signature_item")
        || decl.has_error()
        || decl.is_missing()
        || decl.start_position().row != range.start.line
        || decl.start_position().column != range.start.column
        || decl.end_position().row != range.end.line
        || decl.end_position().column != range.end.column
    {
        return false;
    }
    let mut next_start_row = decl.start_position().row;
    let mut current = decl.prev_named_sibling();
    while let Some(node) = current {
        if node.has_error() || node.is_missing() {
            return false;
        }
        let end = node.end_position();
        let last_row = if end.column == 0 && end.row > node.start_position().row {
            end.row - 1
        } else {
            end.row
        };
        if last_row.saturating_add(1) < next_start_row {
            return false;
        }
        let Ok(text) = node.utf8_text(source) else {
            return false;
        };
        match node.kind() {
            "attribute_item" => {}
            "line_comment" if text.starts_with("///") && !text.starts_with("////") => {}
            "block_comment"
                if text.starts_with("/**")
                    && !text.starts_with("/***")
                    && !text.starts_with("/**/") => {}
            "line_comment" => {
                let before = &source[..node.start_byte()];
                let line_start = before
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |i| i + 1);
                return text.trim_end() == "// astro-sight:allow-dead"
                    && before[line_start..]
                        .iter()
                        .all(|b| matches!(b, b' ' | b'\t'));
            }
            _ => return false,
        }
        next_start_row = node.start_position().row;
        current = node.prev_named_sibling();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    fn marked(source: &str) -> bool {
        let tree = crate::engine::parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let root = tree.root_node();
        let syms =
            crate::engine::symbols::extract_symbols(root, source.as_bytes(), LangId::Rust).unwrap();
        let sym = syms.iter().find(|sym| sym.name == "put").unwrap();
        rust_has_allow_dead_marker(root, source.as_bytes(), &sym.range)
    }

    #[test]
    fn marker_is_exact_adjacent_and_not_inherited() {
        for prefix in [
            "// astro-sight:allow-dead\n",
            "// astro-sight:allow-dead  \r\n",
            "// astro-sight:allow-dead\n/// Write.\n#[inline]\n",
            "// astro-sight:allow-dead\n#[inline]\n/// Write.\n",
            "// astro-sight:allow-dead\n/** Write. */\n",
        ] {
            assert!(
                marked(&format!("{prefix}pub fn put() {{}}\n")),
                "{prefix:?}"
            );
        }
        for prefix in [
            "",
            "// astro-sight:allow-dead\n\n",
            "//astro-sight:allow-dead\n",
            "//  astro-sight:allow-dead\n",
            "/// astro-sight:allow-dead\n",
            "//! astro-sight:allow-dead\n",
            "/* astro-sight:allow-dead */\n",
            "// astro-sight:allow-dead\n// Note.\n",
            "// astro-sight:allow-dead\n/* Note. */\n",
            "// astro-sight:allow-dead\n//! Inner.\n",
            "// astro-sight:allow-dead\n#![allow(dead_code)]\n",
            "pub fn other() {} // astro-sight:allow-dead\n",
            "const TEXT: &str = \"// astro-sight:allow-dead\";\n",
            "const TEXT: &str = r#\"\n// astro-sight:allow-dead\n\"#;\n",
            "// astro-sight:allow-dead\npub fn other() {}\n",
        ] {
            assert!(
                !marked(&format!("{prefix}pub fn put() {{}}\n")),
                "{prefix:?}"
            );
        }
        assert!(!marked(
            "// astro-sight:allow-dead\npub fn put() { let =; }"
        ));
        assert!(!marked(
            "// astro-sight:allow-dead\nimpl Writer { pub fn put(&self) {} }"
        ));
        assert!(!marked(
            "// astro-sight:allow-dead\nmod nested { pub fn put() {} }"
        ));
        assert!(marked(
            "impl Writer {\n // astro-sight:allow-dead\n pub fn put(&self) {}\n}"
        ));
        assert!(marked(
            "// astro-sight:allow-dead\npub fn put() { let _ = \"日本語\"; }"
        ));
    }
}
