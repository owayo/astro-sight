use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use tree_sitter::Node;

use super::python_globals::{BuiltinAliases, ModuleWrites};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Import<'s> {
    package: Option<&'s str>,
    path: &'s str,
}

struct Scope<'s> {
    parent: Option<usize>,
    is_class: bool,
    inside_class: bool,
    imports: HashMap<&'s str, Option<(Import<'s>, usize)>>,
}

pub(super) struct PythonImports<'s> {
    dir: &'s str,
    scopes: Vec<Scope<'s>>,
    receiver_scopes: HashMap<usize, (usize, bool)>,
    module_writes: ModuleWrites,
    written_attributes: HashSet<&'s str>,
    submodules: HashMap<String, bool>,
}

impl<'s> PythonImports<'s> {
    pub(super) fn collect(root: Node<'_>, source: &'s [u8], symbol: &str, dir: &'s str) -> Self {
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
        let mut ancestors = Vec::new();
        let mut module_writes = ModuleWrites::default();
        let mut written_attributes = HashSet::new();
        let builtins = BuiltinAliases::collect(root, source);
        let checked = walk_preorder(root, Children::Named, |node, depth| {
            ancestors.truncate(depth);
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
            if node.kind() == "identifier" {
                let Ok(name) = node.utf8_text(source) else {
                    return ControlFlow::Break(());
                };
                if !name.is_ascii() && !unicode_normalization::is_nfkc(name) {
                    return ControlFlow::Break(());
                }
                let function = builtins.function(name);
                if function == "globals" || (scope == 0 && matches!(function, "vars" | "locals")) {
                    module_writes.observe(node, &ancestors, source, &builtins);
                }
            }
            if node.kind() == "attribute"
                && attribute_is_written(node, &ancestors)
                && let Some(member) = node
                    .child_by_field_name("attribute")
                    .and_then(|n| n.utf8_text(source).ok())
            {
                written_attributes.insert(member);
            }
            ancestors.push(node);
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
                let package = node
                    .child_by_field_name("module_name")
                    .and_then(|n| n.utf8_text(source).ok());
                let mut cursor = node.walk();
                for item in node.children_by_field_name("name", &mut cursor) {
                    let (name, module) = match item.kind() {
                        "aliased_import" => (
                            item.child_by_field_name("alias"),
                            item.child_by_field_name("name"),
                        ),
                        // import a.b が束縛するのは a。from a import b が束縛するのは b。
                        "dotted_name" => (item.named_child(0), item.named_child(0)),
                        _ => continue,
                    };
                    if let (Some(name), Some(module)) = (
                        name.and_then(|n| n.utf8_text(source).ok()),
                        module.and_then(|n| n.utf8_text(source).ok()),
                    ) {
                        let import = Import {
                            package,
                            path: module,
                        };
                        let literal = module.split('.').all(|part| {
                            !part.is_empty()
                                && part.chars().all(|c| c.is_alphanumeric() || c == '_')
                        });
                        let binding = (direct && literal).then_some((import, node.end_byte()));
                        scopes[scope]
                            .imports
                            .entry(name)
                            .and_modify(|prev| {
                                if binding.is_none() || prev.is_some_and(|(old, _)| old != import) {
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
            dir,
            scopes,
            receiver_scopes,
            module_writes,
            written_attributes,
            submodules: HashMap::new(),
        }
    }

    fn import_at(&self, receiver: Node<'_>, base: &str) -> Option<Import<'s>> {
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
                let (import, after) = (*binding)?;
                if after > receiver.start_byte()
                    || (index == 0 && self.module_writes.may_replace(base))
                {
                    return None;
                }
                return Some(import);
            }
            current = scope.parent;
        }
        None
    }

    pub(super) fn module_at(
        &mut self,
        receiver: Node<'_>,
        base: &str,
        source: &[u8],
    ) -> Option<String> {
        let import = self.import_at(receiver, base)?;
        // from で得た値も、モジュールの属性も、同名のサブモジュールとは限らない。
        let mut members = Vec::new();
        let mut object = receiver;
        while object.kind() == "attribute" {
            members.push(
                object
                    .child_by_field_name("attribute")?
                    .utf8_text(source)
                    .ok()?,
            );
            object = object.child_by_field_name("object")?;
        }
        if import.package.is_none() && members.is_empty() {
            return Some(import.path.rsplit('.').next()?.to_owned());
        }
        if members
            .iter()
            .any(|name| self.written_attributes.contains(name))
            || (import.package.is_some() && self.written_attributes.contains(import.path))
        {
            return None;
        }
        let mut path = match import.package {
            Some(package) => format!("{package}.{}", import.path),
            None => import.path.to_owned(),
        };
        for member in members.into_iter().rev() {
            path.push('.');
            path.push_str(member);
        }
        let module = path.rsplit('.').next()?.to_owned();
        let resolved = self.submodules.entry(path).or_insert_with_key(|path| {
            super::super::python_module_path::is_inert_package_submodule(self.dir, path)
        });
        resolved.then_some(module)
    }
}

fn attribute_is_written(attribute: Node<'_>, ancestors: &[Node<'_>]) -> bool {
    for parent in ancestors.iter().rev() {
        let target = match parent.kind() {
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => "left",
            "as_pattern" => "alias",
            "delete_statement" => return true,
            "parenthesized_expression"
            | "pattern_list"
            | "list_pattern"
            | "tuple_pattern"
            | "tuple"
            | "list"
            | "as_pattern_target" => continue,
            _ => return false,
        };
        return parent
            .child_by_field_name(target)
            .is_some_and(|n| n.byte_range().contains(&attribute.start_byte()));
    }
    false
}
