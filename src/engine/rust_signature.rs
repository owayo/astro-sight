//! Rust 関数シグネチャのうち、呼び出し契約に含まれない束縛モードを正規化する。

use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;

/// Rust の通常引数に付く binding-side の `mut` を除いたシグネチャを返す。
///
/// `fn run(mut callback: F)` の `mut` は関数本体で callback 変数を再束縛・可変借用するための
/// 指定で、呼び出し側の型契約ではない。一方、`callback: &mut F` の `mut` は型の一部なので
/// 除外しない。文字列置換では両者を区別できないため、`parameter` の直接子にある
/// `mutable_specifier` だけを AST 位置で除く。
pub(crate) fn normalize_rust_parameter_binding_signature(
    function: Node<'_>,
    source: &[u8],
    start: usize,
    end: usize,
) -> Option<String> {
    let parameters = function
        .child_by_field_name("parameters")
        .or_else(|| direct_named_child(function, "parameters"))?;
    let mut omitted = Vec::new();

    for index in 0..parameters.child_count() {
        let Some(parameter) = parameters.child(index) else {
            continue;
        };
        // `self_parameter` は `mut self` と `&mut self` の grammar shape が異なる版もある。
        // 今回は通常引数の、構造的に型側と区別できる位置だけを扱う。
        if parameter.kind() != "parameter" {
            continue;
        }
        for child_index in 0..parameter.child_count() {
            let Some(child) = parameter.child(child_index) else {
                continue;
            };
            if child.kind() == "mutable_specifier" {
                omitted.push((child.start_byte(), child.end_byte()));
            }
        }
    }

    rebuild_without_ranges(source, start, end, &omitted)
}

/// unified diff の 1 行シグネチャにも同じ AST 正規化を適用する。
/// parse や function node の特定に失敗した場合は `None` を返し、呼び出し側が元文字列で
/// 比較することで fail-closed に倒す。
pub(crate) fn normalize_rust_signature_text(signature: &str) -> Option<String> {
    let original_len = signature.len();
    let trimmed = signature.trim_end();
    let mut parseable = signature.as_bytes().to_vec();
    if trimmed.ends_with('{') {
        parseable.extend_from_slice(b" }");
    } else if !trimmed.ends_with(';') && !trimmed.ends_with('}') {
        parseable.extend_from_slice(b" {}");
    }

    let tree = parser::parse_source(&parseable, LangId::Rust).ok()?;
    let function = find_function_node(tree.root_node())?;
    normalize_rust_parameter_binding_signature(
        function,
        &parseable,
        function.start_byte(),
        original_len,
    )
}

fn direct_named_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn find_function_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "function_item" | "function_signature_item") {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_function_node(child) {
            return Some(found);
        }
    }
    None
}

fn rebuild_without_ranges(
    source: &[u8],
    start: usize,
    end: usize,
    omitted: &[(usize, usize)],
) -> Option<String> {
    let _ = source.get(start..end)?;
    let mut ranges: Vec<_> = omitted
        .iter()
        .copied()
        .filter(|(range_start, range_end)| start <= *range_start && *range_end <= end)
        .collect();
    ranges.sort_unstable();

    let mut rebuilt = Vec::with_capacity(end.saturating_sub(start));
    let mut cursor = start;
    for (range_start, mut range_end) in ranges {
        if range_start < cursor {
            continue;
        }
        rebuilt.extend_from_slice(source.get(cursor..range_start)?);
        while range_end < end && source[range_end].is_ascii_whitespace() {
            range_end += 1;
        }
        cursor = range_end;
    }
    rebuilt.extend_from_slice(source.get(cursor..end)?);
    Some(normalize_whitespace(&rebuilt))
}

fn normalize_whitespace(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_parameter_binding_mut_but_keeps_type_mutability() {
        let with_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(mut callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        let without_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        assert_eq!(with_binding_mut, without_binding_mut);

        let immutable_type =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &u32) {")
                .expect("Rust signature should parse");
        assert_ne!(without_binding_mut, immutable_type);
    }

    #[test]
    fn malformed_signature_fails_closed() {
        assert!(normalize_rust_signature_text("not a Rust function").is_none());
    }
}
