use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use tree_sitter::Node;

struct Scope<'s> {
    parent: Option<usize>,
    is_class: bool,
    inside_class: bool,
    imports: HashMap<&'s str, Option<(&'s str, usize)>>,
}

pub(super) struct PythonImports<'s> {
    scopes: Vec<Scope<'s>>,
    receiver_scopes: HashMap<usize, (usize, bool)>,
}

impl<'s> PythonImports<'s> {
    pub(super) fn collect(root: Node<'_>, source: &'s [u8], symbol: &str) -> Self {
        let mut scopes = vec![Scope {
            parent: None,
            is_class: false,
            inside_class: false,
            imports: HashMap::new(),
        }];
        let mut receiver_scopes = HashMap::new();
        let mut active = Vec::<(usize, usize)>::new();
        let mut bodies = HashMap::new();
        let mut opaque_end = 0;
        let checked = walk_preorder(root, Children::Named, |node, depth| {
            while active.last().is_some_and(|&(level, _)| level >= depth) {
                active.pop();
            }
            let mut scope = active.last().map_or(0, |&(_, index)| index);
            if matches!(node.kind(), "function_definition" | "class_definition")
                && let Some(body) = node.child_by_field_name("body")
            {
                bodies.insert(body.id(), node.kind() == "class_definition");
            }
            if node.kind() == "block"
                && let Some(is_class) = bodies.remove(&node.id())
            {
                // 既定値・注釈・デコレータは外側で評価される。メソッドは class の名前空間を閉じ込めない。
                let outer = if scopes[scope].is_class {
                    scopes[scope].parent
                } else {
                    Some(scope)
                };
                let inside_class = is_class || scopes[scope].inside_class;
                scope = scopes.len();
                scopes.push(Scope {
                    parent: outer,
                    is_class,
                    inside_class,
                    imports: HashMap::new(),
                });
                active.push((depth, scope));
            }
            if node.kind() == "identifier"
                && !node
                    .utf8_text(source)
                    .is_ok_and(|name| name.is_ascii() || unicode_normalization::is_nfkc(name))
            {
                return ControlFlow::Break(());
            }
            // class の中の lambda や内包表記では、式のどの部分かで class の名前の可視性が変わる。
            if scopes[scope].is_class
                && matches!(
                    node.kind(),
                    "lambda"
                        | "list_comprehension"
                        | "set_comprehension"
                        | "dictionary_comprehension"
                        | "generator_expression"
                )
            {
                opaque_end = opaque_end.max(node.end_byte());
            }
            if matches!(node.kind(), "import_statement" | "import_from_statement") {
                let direct = depth == active.last().map_or(1, |&(level, _)| level + 1);
                let mut cursor = node.walk();
                for item in node.children_by_field_name("name", &mut cursor) {
                    let (name, module) = match item.kind() {
                        "aliased_import" => (
                            item.child_by_field_name("alias"),
                            item.child_by_field_name("name").and_then(|path| {
                                let mut cursor = path.walk();
                                path.named_children(&mut cursor)
                                    .filter(|n| n.kind() == "identifier")
                                    .last()
                            }),
                        ),
                        // import a.b が束縛するのは a。from a import b が束縛するのは b。
                        "dotted_name" => (item.named_child(0), item.named_child(0)),
                        _ => continue,
                    };
                    if let (Some(name), Some(module)) = (
                        name.and_then(|n| n.utf8_text(source).ok()),
                        module.and_then(|n| n.utf8_text(source).ok()),
                    ) {
                        let binding = direct.then_some((module, node.end_byte()));
                        scopes[scope]
                            .imports
                            .entry(name)
                            .and_modify(|prev| {
                                if !direct || prev.is_some_and(|(old, _)| old != module) {
                                    *prev = None;
                                }
                            })
                            .or_insert(binding);
                    }
                }
            }
            if node.kind() == "attribute"
                && node
                    .child_by_field_name("attribute")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(symbol)
                && let Some(object) = node.child_by_field_name("object")
            {
                receiver_scopes.insert(object.id(), (scope, node.start_byte() < opaque_end));
            }
            ControlFlow::Continue(Walk::Descend)
        });
        if checked.is_break() {
            receiver_scopes.clear();
        }
        Self {
            scopes,
            receiver_scopes,
        }
    }

    pub(super) fn module_at(&self, receiver: Node<'_>, base: &str) -> Option<&'s str> {
        let &(index, may_skip_class) = self.receiver_scopes.get(&receiver.id())?;
        let innermost = &self.scopes[index];
        if (may_skip_class && innermost.imports.contains_key(base))
            || (innermost.inside_class && base.starts_with("__") && !base.ends_with("__"))
        {
            return None;
        }
        let mut current = Some(index);
        while let Some(index) = current {
            let scope = &self.scopes[index];
            if let Some(binding) = scope.imports.get(base) {
                let (module, after) = (*binding)?;
                return (after <= receiver.start_byte()).then_some(module);
            }
            current = scope.parent;
        }
        None
    }
}
