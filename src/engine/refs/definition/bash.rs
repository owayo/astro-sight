//! Bash の変数の出現と、同名の関数の参照を分離する。

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};
use tree_sitter::Node;

#[derive(Clone, Copy)]
pub(crate) enum BashOccurrence {
    VariableDefinition,
    VariableReference,
    // 宣言コマンドや builtin/command 自体も関数に上書きされ得る。
    DeclarationName,
    FunctionReference,
}

impl BashOccurrence {
    pub(crate) fn is_variable(self) -> bool {
        matches!(self, Self::VariableDefinition | Self::VariableReference)
    }

    pub(crate) fn is_definition(self, occurrences: bool) -> bool {
        matches!(self, Self::VariableDefinition)
            || (occurrences && matches!(self, Self::DeclarationName))
    }
}

#[derive(Default)]
// Node::id は同じ Tree 内でのみ有効。キャッシュは 1 ファイルの walk に閉じる。
pub(crate) struct BashDeclarationCache {
    roles: RefCell<HashMap<usize, BashOccurrence>>,
    overrides: RefCell<Option<HashSet<String>>>,
}

impl BashDeclarationCache {
    fn is_overridden(&self, command: Node<'_>, source: &[u8]) -> bool {
        let name = if command.kind() == "command" {
            command.child_by_field_name("name")
        } else {
            command.child(0)
        }
        .and_then(|n| n.utf8_text(source).ok());
        if self.overrides.borrow().is_none() {
            let mut root = command;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let mut names = HashSet::new();
            // 不完全な構文から「上書きなし」を証明しない。
            if root.has_error() {
                names.insert("*".to_string());
            }
            let mut cursor = root.walk();
            loop {
                let node = cursor.node();
                if node.kind() == "function_definition"
                    && let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                {
                    names.insert(name.to_string());
                }
                if cursor.goto_first_child() {
                    continue;
                }
                while !cursor.goto_next_sibling() {
                    if !cursor.goto_parent() {
                        *self.overrides.borrow_mut() = Some(names);
                        return self.is_overridden(command, source);
                    }
                }
            }
        }
        let overrides = self.overrides.borrow();
        overrides.as_ref().is_some_and(|names| {
            names.contains("*") || name.is_none_or(|name| names.contains(name))
        })
    }
}

/// フラグの走査は宣言コマンドごとに一度だけ。多数の宣言名でも二乗走査にしない。
pub(crate) fn classify_bash_occurrence(
    node: Node<'_>,
    source: &[u8],
    cache: &BashDeclarationCache,
) -> Option<BashOccurrence> {
    if !matches!(node.kind(), "variable_name" | "word") {
        return None;
    }
    let parent = node.parent()?;
    let (command, keyword) = match parent.kind() {
        "declaration_command" | "unset_command" => (Some(parent), None),
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
            (Some(parent), Some(first.utf8_text(source).ok()?))
        }
        _ => (None, None),
    };
    if let Some(command) = command {
        if !node.utf8_text(source).ok().is_some_and(is_variable_name) {
            return None;
        }
        if let Some(role) = cache.roles.borrow().get(&command.id()).copied() {
            return Some(role);
        }
        let role = declaration_role(command, source, keyword);
        let role = match role {
            BashOccurrence::VariableDefinition if !cache.is_overridden(command, source) => {
                BashOccurrence::DeclarationName
            }
            _ => BashOccurrence::FunctionReference,
        };
        cache.roles.borrow_mut().insert(command.id(), role);
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

fn declaration_role(command: Node<'_>, source: &[u8], keyword: Option<&str>) -> BashOccurrence {
    let mut cursor = command.walk();
    // 前置代入・redirect・継続行があっても引数フィールドだけを見る。
    let args: Vec<_> = if keyword.is_some() {
        command
            .children_by_field_name("argument", &mut cursor)
            .skip(1)
            .collect()
    } else {
        command
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .collect()
    };
    let unset = command.kind() == "unset_command" || keyword == Some("unset");
    let mut variable_only = false;
    let mut print_only = false;
    for arg in args {
        if matches!(arg.kind(), "variable_name" | "variable_assignment") {
            break;
        }
        let Ok(text) = arg.utf8_text(source) else {
            return BashOccurrence::FunctionReference;
        };
        let text = match arg.kind() {
            "word" if text.contains('\\') => return BashOccurrence::FunctionReference,
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
