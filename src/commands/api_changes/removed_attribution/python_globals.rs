use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::super::removed_scope::{python_is_load, python_string_value};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Default)]
pub(super) struct BuiltinAliases<'s> {
    modules: HashSet<&'s str>,
    functions: HashMap<&'s str, &'s str>,
}

impl<'s> BuiltinAliases<'s> {
    pub(super) fn collect(root: Node<'_>, source: &'s [u8]) -> Self {
        let mut aliases = Self::default();
        if memchr::memmem::find(source, b"builtins").is_none() {
            return aliases;
        }
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
            let from = node
                .child_by_field_name("module_name")
                .and_then(|n| n.utf8_text(source).ok());
            if node.kind() == "import_statement" || from == Some("builtins") {
                let mut cursor = node.walk();
                for item in node.children_by_field_name("name", &mut cursor) {
                    let path = item
                        .child_by_field_name("name")
                        .unwrap_or(item)
                        .utf8_text(source)
                        .ok();
                    let alias = item
                        .child_by_field_name("alias")
                        .unwrap_or(item)
                        .utf8_text(source)
                        .ok();
                    if let (Some(path), Some(alias)) = (path, alias) {
                        if from == Some("builtins") && matches!(path, "globals" | "vars" | "locals")
                        {
                            aliases
                                .functions
                                .entry(alias)
                                .and_modify(|old| {
                                    if path == "globals" || (*old == "vars" && path == "locals") {
                                        *old = path;
                                    }
                                })
                                .or_insert(path);
                        } else if from.is_none() && path == "builtins" {
                            aliases.modules.insert(alias);
                        }
                    }
                }
                return ControlFlow::Continue(Walk::Skip);
            }
            ControlFlow::Continue(Walk::Descend)
        });
        aliases
    }

    pub(super) fn function<'a>(&'a self, name: &'a str) -> &'a str {
        if name == "globals" {
            "globals"
        } else {
            self.functions.get(name).copied().unwrap_or(name)
        }
    }
}

#[derive(Default)]
pub(super) struct ModuleWrites {
    keys: HashSet<String>,
    unknown: bool,
}

impl ModuleWrites {
    pub(super) fn may_replace(&self, key: &str) -> bool {
        self.unknown || self.keys.contains(key)
    }

    pub(super) fn observe(
        &mut self,
        name: Node<'_>,
        ancestors: &[Node<'_>],
        source: &[u8],
        aliases: &BuiltinAliases<'_>,
    ) {
        if self.unknown {
            return;
        }
        let mut outer = ancestors
            .iter()
            .copied()
            .rev()
            .filter(|n| n.kind() != "parenthesized_expression");
        let encloses_name = |node: Option<Node<'_>>| {
            node.is_some_and(|n| n.byte_range().contains(&name.start_byte()))
        };
        let Some(mut usage) = outer.next() else {
            return;
        };
        if usage.kind() == "attribute" && encloses_name(usage.child_by_field_name("attribute")) {
            let module = usage
                .child_by_field_name("object")
                .and_then(|n| n.utf8_text(source).ok());
            if !module.is_some_and(|m| {
                matches!(m, "builtins" | "__builtins__") || aliases.modules.contains(m)
            }) {
                return;
            }
            let Some(parent) = outer.next() else { return };
            usage = parent;
        } else if matches!(
            usage.kind(),
            "parameters" | "lambda_parameters" | "dotted_name" | "aliased_import"
        ) || encloses_name(usage.child_by_field_name("name"))
            || (matches!(usage.kind(), "assignment" | "augmented_assignment")
                && encloses_name(usage.child_by_field_name("left")))
        {
            return;
        }
        if usage.kind() != "call" || !encloses_name(usage.child_by_field_name("function")) {
            // 組み込み関数そのものが渡されると、どのキーを書き換えるかは追えない。
            self.unknown = true;
            return;
        }
        if name
            .utf8_text(source)
            .ok()
            .is_some_and(|n| aliases.function(n) == "vars")
            && usage.child_by_field_name("arguments").is_some_and(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor).any(|n| {
                    !n.is_extra() && !matches!(n.kind(), "list_splat" | "dictionary_splat")
                })
            })
        {
            // vars(obj) は obj の辞書を返す。引数なしの vars() だけが呼び元の名前空間を選ぶ。
            return;
        }
        match outer.next() {
            Some(usage)
                if usage.kind() == "subscript"
                    && encloses_name(usage.child_by_field_name("value")) =>
            {
                if !python_is_load(usage) {
                    self.record(usage.child_by_field_name("subscript"), source);
                }
            }
            Some(usage)
                if usage.kind() == "attribute"
                    && encloses_name(usage.child_by_field_name("object")) =>
            {
                let method = usage
                    .child_by_field_name("attribute")
                    .and_then(|n| n.utf8_text(source).ok());
                if matches!(
                    method,
                    Some(
                        "get"
                            | "keys"
                            | "items"
                            | "values"
                            | "copy"
                            | "__getitem__"
                            | "__contains__"
                    )
                ) {
                    return;
                }
                let args = outer
                    .next()
                    .filter(|n| {
                        n.kind() == "call" && encloses_name(n.child_by_field_name("function"))
                    })
                    .and_then(|n| n.child_by_field_name("arguments"));
                match (method, args) {
                    (Some("update"), Some(args)) => self.update(args, source),
                    (Some("__setitem__" | "__delitem__" | "setdefault" | "pop"), Some(args)) => {
                        self.record(args.named_child(0), source);
                    }
                    _ => self.unknown = true,
                }
            }
            // 別名への代入、未知の関数への引き渡し、辞書全体の更新。
            _ => self.unknown = true,
        }
    }

    fn update(&mut self, args: Node<'_>, source: &[u8]) {
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor).filter(|n| !n.is_extra()) {
            match arg.kind() {
                "keyword_argument" => {
                    if let Some(name) = arg
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                    {
                        self.keys.insert(name.to_owned());
                    } else {
                        self.unknown = true;
                    }
                }
                "dictionary" => {
                    let mut cursor = arg.walk();
                    for pair in arg.named_children(&mut cursor).filter(|n| !n.is_extra()) {
                        self.record(pair.child_by_field_name("key"), source);
                    }
                }
                _ => self.unknown = true,
            }
        }
    }

    fn record(&mut self, key: Option<Node<'_>>, source: &[u8]) {
        let key = key
            .filter(|n| matches!(n.kind(), "string" | "concatenated_string"))
            .and_then(|n| python_string_value(n, source))
            .filter(|key| !key.contains('\0'));
        if let Some(key) = key {
            self.keys.insert(key);
        } else {
            self.unknown = true;
        }
    }
}
