//! 改行変換に巻き込まれただけの Rust 宣言を、旧新 AST と所属で確かめる。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::engine::{diff, parser, symbols};
use crate::language::LangId;
use crate::models::symbol::Symbol;

type Key<'a> = (&'a str, &'static str);

pub(super) fn unchanged_symbols<'a>(
    file_diff: &str,
    old_path: &str,
    new_path: &str,
    root: Node<'_>,
    source: &[u8],
    new_symbols: &'a [Symbol],
) -> HashSet<Key<'a>> {
    let mut unchanged = HashSet::new();
    // 通常の LF 差分に解析を追加しない。復元不能・別パス・大きい入力は従来の判定を残す。
    if old_path != new_path
        || root.has_error()
        || source.len() > 2 * 1024 * 1024
        || !(source.windows(2).any(|p| p == b"\r\n")
            || file_diff.as_bytes().windows(2).any(|p| p == b"\r\n"))
    {
        return unchanged;
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, new_path, source) else {
        return unchanged;
    };
    if old.source.len() > 2 * 1024 * 1024 {
        return unchanged;
    }
    let Ok(tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return unchanged;
    };
    let old_root = tree.root_node();
    if old_root.has_error() {
        return unchanged;
    }
    let Ok(old_symbols) = symbols::extract_symbols(old_root, &old.source, LangId::Rust) else {
        return unchanged;
    };
    let old_index = unique_symbols(&old_symbols);
    let new_index = unique_symbols(new_symbols);
    let mut old_inner = HashMap::new();
    let mut new_inner = HashMap::new();
    for symbol in new_symbols {
        let key = (symbol.name.as_str(), super::symbol_kind_str(symbol.kind));
        let Some(Some(old_symbol)) = old_index.get(&key) else {
            continue;
        };
        if !new_index.get(&key).is_some_and(Option::is_some)
            || old_symbol.container != symbol.container
        {
            continue;
        }
        let Some(old_node) = declaration(old_root, old_symbol) else {
            continue;
        };
        let Some(new_node) = declaration(root, symbol) else {
            continue;
        };
        if old_node.kind() == new_node.kind()
            && same_text(
                &old.source[old_node.byte_range()],
                &source[new_node.byte_range()],
            )
            && scope(old_node, &old.source, &mut old_inner)
                .zip(scope(new_node, source, &mut new_inner))
                .is_some_and(|(old, new)| old == new)
        {
            unchanged.insert(key);
        }
    }
    unchanged
}

fn unique_symbols(symbols: &[Symbol]) -> HashMap<Key<'_>, Option<&Symbol>> {
    let mut index = HashMap::new();
    for symbol in symbols {
        index
            .entry((symbol.name.as_str(), super::symbol_kind_str(symbol.kind)))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(symbol));
    }
    index
}

fn declaration<'a>(root: Node<'a>, symbol: &Symbol) -> Option<Node<'a>> {
    let start = tree_sitter::Point::new(symbol.range.start.line, symbol.range.start.column);
    let end = tree_sitter::Point::new(symbol.range.end.line, symbol.range.end.column);
    root.named_descendant_for_point_range(start, end)
        .filter(|node| node.start_position() == start && node.end_position() == end)
}

fn normalized(source: &[u8]) -> impl Iterator<Item = u8> + '_ {
    source.iter().enumerate().filter_map(|(i, &byte)| {
        (byte != b'\r' || source.get(i + 1) != Some(&b'\n')).then_some(byte)
    })
}

fn same_text(old: &[u8], new: &[u8]) -> bool {
    normalized(old).eq(normalized(new))
}

fn attributes(mut node: Node<'_>, source: &[u8], out: &mut Vec<Vec<u8>>) {
    while let Some(previous) = node.prev_named_sibling() {
        if !matches!(
            previous.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        ) {
            break;
        }
        let mut end = previous.end_byte();
        // line_comment は CR までを含み LF を範囲外に置く。実ソースの次の LF を確かめる。
        if source.get(end) == Some(&b'\n') && source.get(end.wrapping_sub(1)) == Some(&b'\r') {
            end -= 1;
        }
        out.push(normalized(&source[previous.start_byte()..end]).collect());
        node = previous;
    }
}

/// 所属する module/impl/trait のヘッダ・属性と各スコープの inner attribute を保つ。
/// 本体の別宣言を変更していても対象を比較できるが、所属変更や不明な構造は閉じない。
fn scope(
    mut node: Node<'_>,
    source: &[u8],
    inner_cache: &mut HashMap<usize, Vec<Vec<u8>>>,
) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for _ in 0..128 {
        attributes(node, source, &mut out);
        let parent = node.parent()?;
        // 同じスコープの全 sibling を宣言ごとに再走査しない。
        let inner = inner_cache.entry(parent.id()).or_insert_with(|| {
            let mut cursor = parent.walk();
            parent
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "inner_attribute_item")
                .map(|n| normalized(&source[n.byte_range()]).collect())
                .collect()
        });
        if out.iter().chain(inner.iter()).map(Vec::len).sum::<usize>() > 64 * 1024 {
            return None;
        }
        out.extend(inner.iter().cloned());
        if parent.kind() == "source_file" {
            return Some(out);
        }
        if parent.kind() != "declaration_list" {
            return None;
        }
        let owner = parent.parent()?;
        if owner.child_by_field_name("body")?.id() != parent.id() {
            return None;
        }
        out.push(owner.kind().as_bytes().to_vec());
        out.push(normalized(&source[owner.start_byte()..parent.start_byte()]).collect());
        out.push(normalized(&source[parent.end_byte()..owner.end_byte()]).collect());
        node = owner;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(old: &str, new: &str) -> String {
        let mut text = format!(
            "--- a/api.rs\n+++ b/api.rs\n@@ -1,{} +1,{} @@\n",
            old.lines().count(),
            new.lines().count()
        );
        for (prefix, source) in [('-', old), ('+', new)] {
            for line in source.split_inclusive('\n') {
                text.push(prefix);
                text.push_str(line);
            }
        }
        text
    }

    fn unchanged(old: &str, new: &str) -> HashSet<String> {
        let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
        let symbols =
            symbols::extract_symbols(tree.root_node(), new.as_bytes(), LangId::Rust).unwrap();
        unchanged_symbols(
            &patch(old, new),
            "api.rs",
            "api.rs",
            tree.root_node(),
            new.as_bytes(),
            &symbols,
        )
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect()
    }

    #[test]
    fn rust_eol_proof_compares_nested_owners_attributes_and_literals() {
        let old = "#![allow(dead_code)]\n/// Row の説明\npub struct Row;\n// 実装の説明\nimpl Row { pub fn build() -> Self { Self } }\npub trait Action { fn apply(&self); }\npub mod nested { pub const VALUE: &str = r#\"a\nb\"#; }\n";
        let new = old.replace('\n', "\r\n");
        let names = unchanged(old, &new);
        for name in ["Row", "build", "Action", "apply", "nested", "VALUE"] {
            assert!(names.contains(name), "{name}: {names:?}");
        }
        for (old, new, target) in [
            (
                "#[cfg(a)]\npub struct Row;\n",
                "#[cfg(b)]\npub struct Row;\r\n",
                "Row",
            ),
            (
                "pub mod one { pub struct Row; }\n",
                "pub mod two { pub struct Row; }\r\n",
                "Row",
            ),
            (
                "pub struct A; impl A { pub fn build() {} }\n",
                "pub struct B; impl B { pub fn build() {} }\r\n",
                "build",
            ),
            (
                "#![cfg(a)]\npub struct Row;\n",
                "#![cfg(b)]\npub struct Row;\r\n",
                "Row",
            ),
            (
                "pub const VALUE: &str = \"a b\";\n",
                "pub const VALUE: &str = \"a  b\";\r\n",
                "VALUE",
            ),
            (
                "pub const VALUE: &str = r#\"a\rb\"#;\n",
                "pub const VALUE: &str = r#\"ab\"#;\r\n",
                "VALUE",
            ),
        ] {
            assert!(!unchanged(old, new).contains(target), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn rust_eol_proof_keeps_ambiguous_and_unreconstructable_inputs() {
        let old = "mod one { pub fn run() {} }\nmod two { pub fn run() {} }\n";
        assert!(!unchanged(old, &old.replace('\n', "\r\n")).contains("run"));
        assert!(unchanged("pub struct Row;\n", "pub struct Row;\r\nfn broken(\n").is_empty());
        assert!(unchanged("pub struct Row;\nfn broken(\n", "pub struct Row;\r\n").is_empty());
        let source = b"pub struct Row;\r\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let symbols = symbols::extract_symbols(tree.root_node(), source, LangId::Rust).unwrap();
        for (diff, old_path) in [
            (
                patch("pub struct Row;\n", "pub struct Row;\r\n"),
                "other.rs",
            ),
            (
                patch("pub struct Row;\n", "pub struct Wrong;\r\n"),
                "api.rs",
            ),
        ] {
            assert!(
                unchanged_symbols(
                    &diff,
                    old_path,
                    "api.rs",
                    tree.root_node(),
                    source,
                    &symbols
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn rust_eol_scope_node_kinds_exist() {
        let language = LangId::Rust.ts_language();
        for kind in [
            "attribute_item",
            "inner_attribute_item",
            "line_comment",
            "block_comment",
            "source_file",
            "declaration_list",
        ] {
            tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
        }
    }
}
