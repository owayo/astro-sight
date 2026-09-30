//! Bash の変数の出現と、同名の関数の参照を分離する。

use std::{cell::RefCell, collections::HashMap};
use tree_sitter::Node;

#[derive(Clone, Copy)]
pub(crate) enum BashOccurrence {
    VariableDefinition,
    VariableReference,
    FunctionReference,
}

impl BashOccurrence {
    pub(crate) fn is_variable(self) -> bool {
        !matches!(self, Self::FunctionReference)
    }

    pub(crate) fn is_definition(self) -> bool {
        matches!(self, Self::VariableDefinition)
    }
}

#[derive(Default)]
pub(crate) struct BashDeclarationCache(RefCell<HashMap<usize, BashOccurrence>>);

/// フラグの走査は宣言コマンドごとに一度だけ。多数の宣言名でも二乗走査にしない。
pub(crate) fn classify_bash_occurrence(
    node: Node<'_>,
    source: &[u8],
    cache: &BashDeclarationCache,
) -> Option<BashOccurrence> {
    let parent = node.parent()?;
    if !matches!(node.kind(), "variable_name" | "word") {
        return None;
    }
    let (command, skip) = match parent.kind() {
        "declaration_command" | "unset_command" => (Some(parent), 0),
        "command" => {
            let name = parent.child_by_field_name("name")?.utf8_text(source).ok()?;
            if !matches!(name, "builtin" | "command") {
                return None;
            }
            let mut cursor = parent.walk();
            let first = parent
                .children_by_field_name("argument", &mut cursor)
                .next()?;
            if first.id() == node.id()
                || !matches!(
                    first.utf8_text(source).ok()?,
                    "local" | "export" | "declare" | "typeset" | "readonly" | "unset"
                )
            {
                return None;
            }
            (Some(parent), 1)
        }
        _ => (None, 0),
    };
    if let Some(command) = command {
        if !node.utf8_text(source).ok().is_some_and(is_variable_name) {
            return None;
        }
        if let Some(role) = cache.0.borrow().get(&command.id()).copied() {
            return Some(role);
        }
        let role = declaration_role(command, source, skip);
        cache.0.borrow_mut().insert(command.id(), role);
        return Some(role);
    }
    if node.kind() != "variable_name" {
        return None;
    }
    let (target, parent) = if parent.kind() == "subscript"
        && parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id())
    {
        (parent, parent.parent()?)
    } else {
        (node, parent)
    };
    if (parent.kind() == "variable_assignment"
        && parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == target.id()))
        || (parent.kind() == "for_statement"
            && parent
                .child_by_field_name("variable")
                .is_some_and(|n| n.id() == target.id()))
    {
        return Some(BashOccurrence::VariableDefinition);
    }
    // ERROR 内の曖昧な名前は callable 側に保持し、破壊的削除の見逃しを作らない。
    Some(if parent.is_error() {
        BashOccurrence::FunctionReference
    } else {
        BashOccurrence::VariableReference
    })
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == b'_' || c.is_ascii_alphanumeric())
}

fn declaration_role(command: Node<'_>, source: &[u8], skip: usize) -> BashOccurrence {
    let mut cursor = command.walk();
    let mut children = command.named_children(&mut cursor);
    // builtin/command の command_name と、その直後の宣言コマンド名を飛ばす。
    if skip != 0 {
        children.next();
    }
    let mut args = children.skip(skip).filter(|n| n.kind() != "comment");
    let unset = command.kind() == "unset_command"
        || (skip != 0
            && command
                .utf8_text(source)
                .ok()
                .is_some_and(|s| s.split_whitespace().nth(1) == Some("unset")));
    let mut variable_only = false;
    let mut print_only = false;
    for arg in &mut args {
        if matches!(arg.kind(), "variable_name" | "variable_assignment") {
            break;
        }
        let Ok(text) = arg.utf8_text(source) else {
            return BashOccurrence::FunctionReference;
        };
        let text = match arg.kind() {
            "word" => text,
            "raw_string" => text.trim_matches('\''),
            "string"
                if {
                    let mut cursor = arg.walk();
                    arg.named_children(&mut cursor)
                        .all(|child| child.kind() == "string_content")
                } =>
            {
                text.trim_matches('"')
            }
            _ => return BashOccurrence::FunctionReference,
        };
        if text == "--" || !text.starts_with(['-', '+']) {
            break;
        }
        if text[1..].contains(['f', 'F']) {
            return BashOccurrence::FunctionReference;
        }
        variable_only |= text[1..].contains(['v', 'n']);
        print_only |= text[1..].contains('p');
    }
    // unset NAME は変数が無いと関数を消す。-v/-n のときだけ変数と証明できる。
    if unset && !variable_only {
        BashOccurrence::FunctionReference
    } else if unset || print_only {
        BashOccurrence::VariableReference
    } else {
        BashOccurrence::VariableDefinition
    }
}
