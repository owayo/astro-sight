//! 宣言行の外にある extern ブロックの ABI 変更を、その直下の関数に帰属させる。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::{Node, Point};

use super::{DeclarationChangeInput, LazyOldSide};
use crate::engine::rust_signature::foreign::Block;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::{LangId, normalize_identifier};
use crate::models::impact::SignatureChange;

#[derive(Default)]
pub(in crate::engine::impact) struct ForeignChanges {
    pub(in crate::engine::impact) changed: HashMap<Point, SignatureChange>,
    // ABI の見出し以外はファイル全体が同じとき、同じ行にある別宣言まで変更扱いしない。
    pub(in crate::engine::impact) unchanged: HashSet<Point>,
}

struct ModuleBlock<'tree, 'src> {
    module: String,
    block: Block<'tree, 'src>,
}

fn module_blocks<'tree, 'src>(
    root: Node<'tree>,
    source: &'src [u8],
) -> Vec<ModuleBlock<'tree, 'src>> {
    let mut blocks = Vec::new();
    let mut modules = Vec::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        while modules.last().is_some_and(|(level, _)| *level >= depth) {
            modules.pop();
        }
        let walk = match node.kind() {
            "source_file" | "declaration_list" => Walk::Descend,
            "mod_item" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    modules.push((depth, normalize_identifier(LangId::Rust, name)));
                }
                Walk::Descend
            }
            "foreign_mod_item" => {
                if let Some(block) = Block::read(node, source) {
                    blocks.push(ModuleBlock {
                        module: modules
                            .iter()
                            .map(|(_, name)| name.as_ref())
                            .collect::<Vec<_>>()
                            .join("::"),
                        block,
                    });
                }
                Walk::Skip
            }
            _ => Walk::Skip,
        };
        ControlFlow::Continue(walk)
    });
    blocks
}

fn only_headers_changed(
    old: &[ModuleBlock<'_, '_>],
    before: &[u8],
    new: &[ModuleBlock<'_, '_>],
    after: &[u8],
) -> bool {
    let mut left = 0;
    let mut right = 0;
    old.len() == new.len()
        && old.iter().zip(new).all(|(old, new)| {
            let equal = super::super::rust_line_endings::same_text(
                &before[left..old.block.modifier.start_byte()],
                &after[right..new.block.modifier.start_byte()],
            );
            left = old.block.body.start_byte();
            right = new.block.body.start_byte();
            equal
        })
        && super::super::rust_line_endings::same_text(&before[left..], &after[right..])
}

#[cfg(test)]
#[test]
fn foreign_pointers_require_an_independent_link_and_unchanged_context() {
    let source = r#"pub mod api { unsafe extern "C" { #[link_name="main"] pub fn f(x:u8); } }
pub mod other { unsafe extern "C" { #[link_name="second"] pub fn f(x:u8); } }"#;
    let prove = |source: &str| {
        let changed = source.replacen("f(x:u8)", "f(x:i32)", 1);
        unchanged_foreign_pointers(source.as_bytes(), changed.as_bytes()).unwrap_or_default()
    };
    assert_eq!(prove(source), HashSet::from(["other::f".to_owned()]));
    for another in ["main", r"\x6dain", r"\u{6d}ain"] {
        assert!(prove(&source.replace("second", another)).is_empty());
    }
    assert!(
        prove(&source.replace(
            "#[link_name=\"second\"]",
            "#[link_name=\"main\"] #[link_name=\"second\"]"
        ))
        .is_empty()
    );
    assert!(prove(&source.replace("pub mod other", "#[cfg(all())] pub mod other")).is_empty());
    let both = source.replace("f(x:u8)", "f(x:i32)");
    assert!(
        unchanged_foreign_pointers(source.as_bytes(), both.as_bytes())
            .unwrap()
            .is_empty()
    );
}

/// ソース全体で、無条件の外部関数の引数だけを変更しているか。
/// 同じリンク名を共有する別宣言も、そのリンク名が変化した場合は除外の証拠にしない。
pub(in crate::engine::impact) fn unchanged_foreign_pointers(
    before: &[u8],
    after: &[u8],
) -> Option<HashSet<String>> {
    struct Member<'a> {
        path: String,
        link: &'a str,
        parameters: std::ops::Range<usize>,
    }
    fn members(source: &[u8]) -> Option<Vec<Member<'_>>> {
        let tree = crate::engine::parser::parse_source(source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error()
            || crate::engine::tree_walk::any_node(root, Children::Named, |n| {
                matches!(n.kind(), "inner_attribute_item" | "attribute_item")
                    && link_name(n, source).is_none()
            })
        {
            return None;
        }
        let mut members = Vec::new();
        for owner in module_blocks(root, source) {
            let mut link = None;
            for item in owner
                .block
                .body
                .named_children(&mut owner.block.body.walk())
            {
                match item.kind() {
                    "line_comment" | "block_comment" => continue,
                    "attribute_item" => {
                        if link.replace(link_name(item, source)?).is_some() {
                            return None;
                        }
                    }
                    "function_signature_item" => {
                        let name = item
                            .child_by_field_name("name")?
                            .utf8_text(source)
                            .ok()?
                            .trim_start_matches("r#");
                        if !name.is_ascii() {
                            return None;
                        }
                        let path = if owner.module.is_empty() {
                            name.to_owned()
                        } else {
                            format!("{}::{name}", owner.module)
                        };
                        members.push(Member {
                            path,
                            link: link.take().unwrap_or(name),
                            parameters: item.child_by_field_name("parameters")?.byte_range(),
                        });
                    }
                    _ => {
                        link = None;
                    }
                }
            }
        }
        Some(members)
    }
    fn link_name<'a>(item: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
        let attr = item.named_child(0)?;
        if attr.named_child(0)?.utf8_text(source).ok()? != "link_name" {
            return None;
        }
        let value = attr.child_by_field_name("value")?;
        let text = value
            .utf8_text(source)
            .ok()?
            .strip_prefix('"')?
            .strip_suffix('"')?;
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .then_some(text)
    }
    let left = members(before)?;
    let right = members(after)?;
    if left.len() != right.len() {
        return None;
    }
    let mut old_end = 0;
    let mut new_end = 0;
    let mut changed_links = HashSet::new();
    let same = super::super::rust_line_endings::same_text;
    for (old, new) in left.iter().zip(&right) {
        if old.path != new.path
            || old.link != new.link
            || !same(
                &before[old_end..old.parameters.start],
                &after[new_end..new.parameters.start],
            )
        {
            return None;
        }
        if !same(
            &before[old.parameters.clone()],
            &after[new.parameters.clone()],
        ) {
            changed_links.insert(new.link);
        }
        old_end = old.parameters.end;
        new_end = new.parameters.end;
    }
    if changed_links.is_empty() || !same(&before[old_end..], &after[new_end..]) {
        return None;
    }
    Some(
        right
            .into_iter()
            .filter(|member| !changed_links.contains(member.link))
            .map(|member| member.path)
            .collect(),
    )
}

impl ForeignChanges {
    fn record(
        &mut self,
        old: &Block<'_, '_>,
        previous: Node<'_>,
        new: &Block<'_, '_>,
        member: Node<'_>,
    ) {
        if let Some(name) = member
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(new.source).ok())
            && let Some(old_signature) = old.signature(previous)
            && let Some(new_signature) = new.signature(member)
        {
            self.changed.insert(
                member.start_position(),
                SignatureChange {
                    name: name.to_owned(),
                    old_signature,
                    new_signature,
                },
            );
        }
    }

    pub(in crate::engine::impact) fn collect(
        input: &DeclarationChangeInput<'_>,
        old_side: &LazyOldSide,
    ) -> Self {
        let mut result = Self::default();
        if input.lang_id != LangId::Rust || memchr::memmem::find(input.source, b"extern").is_none()
        {
            return result;
        }
        let new = module_blocks(input.root, input.source);
        if new.is_empty() {
            return result;
        }
        let mut changed_lines: Vec<_> = input
            .facts
            .added_lines
            .iter()
            .chain(&input.facts.deletion_gaps)
            .copied()
            .collect();
        changed_lines.sort_unstable();
        let touched = |block: &Block<'_, '_>| {
            let start = block.modifier.start_position().row;
            let end = block.body.end_position().row;
            changed_lines
                .get(changed_lines.partition_point(|&line| line < start))
                .is_some_and(|&line| line <= end)
        };
        if !new.iter().any(|item| touched(&item.block)) {
            return result;
        }
        let Some(old) = old_side.get(input) else {
            return result;
        };
        let before = module_blocks(old.tree.root_node(), &old.source);
        let only_headers = input.old_file_path == input.file_path
            && !input.root.has_error()
            && !old.tree.root_node().has_error()
            && only_headers_changed(&before, &old.source, &new, input.source);
        if only_headers {
            // 全てのブロックの本体がそのままなら、同名の cfg 別宣言も順に一対一で対応する。
            for (previous, current) in before
                .iter()
                .zip(&new)
                .filter(|(_, current)| touched(&current.block))
            {
                let mut left = previous.block.body.walk();
                let mut right = current.block.body.walk();
                for (old_member, new_member) in previous
                    .block
                    .body
                    .named_children(&mut left)
                    .zip(current.block.body.named_children(&mut right))
                {
                    match new_member.kind() {
                        "function_signature_item" if previous.block.abi != current.block.abi => {
                            result.record(&previous.block, old_member, &current.block, new_member)
                        }
                        "function_signature_item" | "static_item" => {
                            result.unchanged.insert(new_member.start_position());
                        }
                        _ => {}
                    }
                }
            }
            return result;
        }
        // 変更行を除いて数えた位置で、cfg 分岐の同名宣言も元の行に対応付けられる。
        let mut removed: Vec<_> = old.removed_lines.iter().copied().collect();
        removed.sort_unstable();
        let mut added: Vec<_> = input.facts.added_lines.iter().copied().collect();
        added.sort_unstable();
        let unchanged_point = |point: Point, edits: &[usize]| {
            edits
                .binary_search(&point.row)
                .err()
                .map(|before| Point::new(point.row - before, point.column))
        };
        let mut old_by_name: HashMap<_, Vec<_>> = HashMap::new();
        for (i, item) in before.iter().enumerate() {
            let mut cursor = item.block.body.walk();
            for function in item
                .block
                .body
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "function_signature_item")
            {
                if let Some(name) = function
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(&old.source).ok())
                {
                    old_by_name
                        .entry((
                            item.module.as_str(),
                            normalize_identifier(LangId::Rust, name),
                        ))
                        .or_default()
                        .push((
                            i,
                            function,
                            unchanged_point(function.start_position(), &removed),
                        ));
                }
            }
        }
        for item in new.iter().filter(|item| touched(&item.block)) {
            let block = &item.block;
            let mut cursor = block.body.walk();
            for member in block.body.named_children(&mut cursor) {
                let pos = member.start_position();
                if member.kind() != "function_signature_item" {
                    continue;
                }
                let Some(name) = member
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(input.source).ok())
                else {
                    continue;
                };
                let Some(entries) = old_by_name.get(&(
                    item.module.as_str(),
                    normalize_identifier(LangId::Rust, name),
                )) else {
                    continue;
                };
                let previous = unchanged_point(pos, &added)
                    .and_then(|point| entries.iter().find(|(_, _, pos)| *pos == Some(point)))
                    .or_else(|| {
                        entries
                            .iter()
                            .find(|(j, _, _)| before[*j].block.abi != block.abi)
                    });
                if let Some((index, previous, _)) = previous
                    && before[*index].block.abi != block.abi
                {
                    result.record(&before[*index].block, *previous, block, member);
                }
            }
        }
        result
    }
}
