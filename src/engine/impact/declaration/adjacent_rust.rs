//! 同じ変更行の別の項目に巻き込まれた Rust の宣言を旧新の構文から区別する。

use super::{DeclarationChangeInput, LazyOldSide};
use crate::engine::diff::{FileDiffLine, FileDiffLines, HunkBodyLine};
use crate::engine::impact::rust_path_owner::RustPathOwnerProjectProof;
use crate::engine::impact::rust_std_origin::RustStdOriginIndex;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;
use std::cell::{LazyCell, OnceCell};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use tree_sitter::{Node, Point};

type RowItems<'a> = HashMap<(&'a str, &'a str), (Node<'a>, &'a [u8])>;

struct Row<'a> {
    before: &'a [u8],
    after: &'a [u8],
    items: RowItems<'a>,
}

fn row<'a>(root: Node<'a>, source: &'a [u8], line: usize) -> Option<Row<'a>> {
    if root.has_error() {
        return None;
    }
    let mut items = HashMap::new();
    let mut extent: Option<std::ops::Range<usize>> = None;
    walk_preorder(root, Children::Named, |node, _| {
        if node.start_position().row > line || node.end_position().row < line {
            return ControlFlow::Continue(Walk::Skip);
        }
        match node.kind() {
            "source_file" | "declaration_list" | "mod_item" => ControlFlow::Continue(Walk::Descend),
            "struct_item" | "function_item" | "const_item" | "type_item"
                if node.start_position().row == line && node.end_position().row == line =>
            {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                else {
                    return ControlFlow::Break(());
                };
                if let Some(span) = &mut extent {
                    if !source[span.end..node.start_byte()]
                        .iter()
                        .all(u8::is_ascii_whitespace)
                    {
                        return ControlFlow::Break(());
                    }
                    span.end = node.end_byte();
                } else {
                    extent = Some(node.byte_range());
                }
                if items
                    .insert(
                        (node.kind(), name.trim_start_matches("r#")),
                        (node, &source[node.byte_range()]),
                    )
                    .is_some()
                {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Walk::Skip)
            }
            _ => ControlFlow::Continue(Walk::Skip),
        }
    })
    .is_continue()
    .then_some(())?;
    let span = extent?;
    // 属性マクロは同じ行の別宣言の内容や位置も、自分が生成する項目の入力にできる。
    if memchr::memchr2(b'#', b'!', source).is_some()
        && any_node(root, Children::Named, |n| {
            (matches!(n.kind(), "macro_invocation" | "macro_definition")
                && n.start_position().row <= line
                && n.end_position().row >= line)
                || matches!(n.kind(), "attribute_item" | "inner_attribute_item")
                    && !super::super::rust_std_origin::safe_attribute(n, source)
                        .is_some_and(|names| names.is_empty())
        })
    {
        return None;
    }
    Some(Row {
        before: source[..span.start]
            .rsplit(|&b| b == b'\n')
            .next()?
            .trim_ascii_end(),
        after: source[span.end..]
            .split(|&b| b == b'\n')
            .next()?
            .trim_ascii(),
        items,
    })
}

#[derive(Default)]
pub(in crate::engine::impact) struct AdjacentDeclarations {
    pub(in crate::engine::impact) unchanged: HashSet<Point>,
    pub(in crate::engine::impact) signatures: HashMap<String, (String, String)>,
}

/// 同じ行の変更が独立した項目だけに収まる場合に限る。型別名や配列長の定数の変更が
/// 字面の同じ構造体に波及する場合は、行ベースの候補を残す。
pub(in crate::engine::impact) fn compare_neighbors<'p>(
    input: &DeclarationChangeInput<'_>,
    old_side: &LazyOldSide,
    layout: impl FnOnce() -> Option<&'p RustPathOwnerProjectProof>,
) -> AdjacentDeclarations {
    let mut result = AdjacentDeclarations::default();
    if input.lang_id != LangId::Rust
        || !input.complete_single_file_diff
        || input.facts.added_lines.len() != 1
        || !FileDiffLines::new(input.file_diff, input.file_path).any(|l| match l {
            FileDiffLine::Body(HunkBodyLine::Added(s) | HunkBodyLine::Removed(s)) => {
                ["struct", "fn", "const", "type"]
                    .into_iter()
                    .flat_map(|word| s.matches(word))
                    .nth(1)
                    .is_some()
            }
            _ => false,
        })
    {
        return result;
    }
    let line = *input.facts.added_lines.iter().next().unwrap();
    let Some(new) = row(input.root, input.source, line) else {
        return result;
    };
    let Some(previous) = old_side.get(input) else {
        return result;
    };
    if previous.removed_lines.len() != 1 || !previous.removed_lines.contains(&line) {
        return result;
    }
    let Some(old) = row(previous.tree.root_node(), &previous.source, line) else {
        return result;
    };
    if old.items.len().max(new.items.len()) < 2
        || old.before != new.before
        || old.after != new.after
    {
        return result;
    }
    let mut changed_names = HashSet::new();
    for (side, other) in [(&old, &new), (&new, &old)] {
        for (&key, &(node, text)) in &side.items {
            if other
                .items
                .get(&key)
                .is_some_and(|&(_, prior)| prior == text)
            {
                continue;
            }
            // 関数内の impl やマクロも外の名前・型の契約を変え得る。
            if any_node(node, Children::Named, |n| {
                matches!(
                    n.kind(),
                    "impl_item"
                        | "macro_invocation"
                        | "macro_definition"
                        | "attribute_item"
                        | "inner_attribute_item"
                )
            }) {
                return result;
            }
            changed_names.insert(key.1);
        }
    }
    // include! 先の外で定義した同名の型は、ファイル内の名前索引には載らない。
    let ordinary = LazyCell::new(|| {
        let file = Path::new(input.file_path);
        layout().is_some_and(|project| {
            project.ordinary_private_module(file).is_some() && project.unrelocated(file)
        })
    });
    let old_names = OnceCell::new();
    let new_names = OnceCell::new();
    let mut old_inner = HashMap::new();
    let mut new_inner = HashMap::new();
    for (&key, &(node, text)) in &new.items {
        let Some(&(old_node, old_text)) = old.items.get(&key) else {
            continue;
        };
        if old_text != text {
            if key.0 == "function_item" {
                let signature = |n: Node<'_>, src: &[u8]| {
                    crate::engine::rust_signature::normalize_rust_parameter_binding_signature(
                        n,
                        src,
                        n.start_byte(),
                        n.end_byte(),
                    )
                };
                if let (Some(old_signature), Some(new_signature)) = (
                    signature(old_node, &previous.source),
                    signature(node, input.source),
                ) && old_signature != new_signature
                {
                    result
                        .signatures
                        .insert(key.1.to_owned(), (old_signature, new_signature));
                }
            }
            continue;
        }
        if independent(node, input.source, |name| {
            !changed_names.contains(name)
                && *ordinary
                && old_names
                    .get_or_init(|| {
                        RustStdOriginIndex::from_source(previous.tree.root_node(), &previous.source)
                    })
                    .as_ref()
                    .is_some_and(|names| names.unshadowed_item_name(old_node, name))
                && new_names
                    .get_or_init(|| RustStdOriginIndex::from_source(input.root, input.source))
                    .as_ref()
                    .is_some_and(|names| names.unshadowed_item_name(node, name))
        }) && super::super::rust_line_endings::scope(old_node, &previous.source, &mut old_inner)
            .zip(super::super::rust_line_endings::scope(
                node,
                input.source,
                &mut new_inner,
            ))
            .is_some_and(|(a, b)| a == b)
        {
            result.unchanged.insert(node.start_position());
        }
    }
    result
}

/// 型別名・定数・マクロ・opaque 型などを含まず、変更した名前にも頼らない宣言。
fn independent(node: Node<'_>, source: &[u8], primitive: impl Fn(&str) -> bool) -> bool {
    let name = node.child_by_field_name("name");
    !any_node(node, Children::Named, |n| {
        if Some(n) == name || n.is_extra() {
            return false;
        }
        match n.kind() {
            "struct_item"
            | "const_item"
            | "type_item"
            | "function_item"
            | "visibility_modifier"
            | "parameters"
            | "parameter"
            | "block"
            | "unit_type"
            | "ordered_field_declaration_list"
            | "field_declaration_list"
            | "field_declaration"
            | "field_identifier"
            | "integer_literal"
            | "float_literal"
            | "boolean_literal"
            | "char_literal" => false,
            "primitive_type" => !n.utf8_text(source).is_ok_and(&primitive),
            _ => true,
        }
    })
}
