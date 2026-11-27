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
