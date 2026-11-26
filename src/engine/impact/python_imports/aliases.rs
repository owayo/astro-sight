//! from-import の別名を直接呼ぶ位置を、変更元のモジュールと字句スコープに帰属させる。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::python_scope::{collect_targets, is_comprehension, parameter_target};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Clone, Copy)]
struct Binding {
    owner: u32,
    symbol: u32,
    after: usize,
    conditional_suite: Option<usize>,
    multiline_site: Option<(u32, u32)>,
}

#[derive(Default)]
struct Scope<'a> {
    parent: Option<usize>,
    aliases: HashMap<&'a str, Binding>,
    shadowed: HashSet<String>,
}

enum Callee {
    Imported(Binding),
    Other,
    Unresolved,
}

fn direct_callee(node: Node<'_>) -> Option<Node<'_>> {
    let mut callee = node.child_by_field_name("function")?;
    while callee.kind() == "parenthesized_expression" {
        let mut cursor = callee.walk();
        callee = callee
            .named_children(&mut cursor)
            .find(|child| !child.is_extra())?;
    }
    (callee.kind() == "identifier").then_some(callee)
}

fn imported_aliases<'a>(
    root: Node<'_>,
    source: &'a [u8],
    resolve: &mut impl FnMut(&str, &str) -> Option<(u32, u32)>,
) -> HashMap<usize, (&'a str, Binding)> {
    let mut imports = HashMap::new();
    let scanned = walk_preorder(root, Children::Named, |node, _| {
        // Python は名前を NFKC に正規化する。別の綴りで同じ束縛を作れるファイルは保留する。
        if node.kind() == "identifier"
            && node
                .utf8_text(source)
                .is_ok_and(|s| !s.is_ascii() && !unicode_normalization::is_nfkc(s))
        {
            return ControlFlow::Break(());
        }
        if node.kind() == "import_from_statement"
            && let Some(module) = node
                .child_by_field_name("module_name")
                .and_then(|n| n.utf8_text(source).ok())
        {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                if let Some(local) = name
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    && let Some(remote) = name
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                    && local != remote
                    && let Some((owner, symbol)) = resolve(module, remote)
                {
                    imports.insert(
                        name.id(),
                        (
                            local,
                            Binding {
                                owner,
                                symbol,
                                after: node.end_byte(),
                                conditional_suite: None,
                                multiline_site: (name.start_position().row
                                    != node.start_position().row)
                                    .then(|| {
                                        let p = name.start_position();
                                        (p.row as u32, p.column as u32)
                                    }),
                            },
                        ),
                    );
                }
            }
        }
        ControlFlow::<(), _>::Continue(Walk::Descend)
    });
    if scanned.is_break() {
        imports.clear();
    }
    imports
}

/// 複数行 import を情報扱いにする前に、別名の値がコールバックなどへ渡る例を外す。
fn multiline_import_sites(
    root: Node<'_>,
    source: &[u8],
    imports: &HashMap<usize, (&str, Binding)>,
    unresolved: HashSet<&str>,
) -> HashSet<(u32, u32)> {
    let names: HashSet<_> = imports
        .values()
        .filter_map(|(name, b)| b.multiline_site.map(|_| *name))
        .collect();
    if names.is_empty() {
        return HashSet::new();
    }
    let mut handled = HashSet::new();
    let mut value_uses = HashSet::new();
    let _ = walk_preorder(root, Children::Named, |node, _| {
        if handled.remove(&node.id()) {
            return ControlFlow::Continue(Walk::Skip);
        }
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                return ControlFlow::Continue(Walk::Skip);
            }
            "call" => handled.extend(direct_callee(node).map(|n| n.id())),
            "function_definition" | "class_definition" | "keyword_argument" => {
                handled.extend(node.child_by_field_name("name").map(|n| n.id()));
            }
            "attribute" => handled.extend(node.child_by_field_name("attribute").map(|n| n.id())),
            "parameters" | "lambda_parameters" => {
                let mut cursor = node.walk();
                handled.extend(
                    node.named_children(&mut cursor)
                        .filter_map(parameter_target)
                        .map(|n| n.id()),
                );
            }
            "identifier"
                if let Ok(name) = node.utf8_text(source)
                    && names.contains(name) =>
            {
                value_uses.insert(name);
            }
            _ => {}
        }
        ControlFlow::<(), _>::Continue(Walk::Descend)
    });
    imports
        .values()
        .filter_map(|(name, b)| {
            b.multiline_site
                .filter(|_| !value_uses.contains(name) && !unresolved.contains(name))
        })
        .collect()
}

impl<'a> Scope<'a> {
    fn record_import(
        &mut self,
        node: Node<'_>,
        source: &[u8],
        conditional_suite: Option<usize>,
        imports: &HashMap<usize, (&'a str, Binding)>,
    ) {
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            if let Some(&(local, mut binding)) = imports.get(&name.id()) {
                binding.conditional_suite = conditional_suite;
                if self.aliases.insert(local, binding).is_some() {
                    self.shadowed.insert(local.to_owned());
                }
            } else if let Some(target) = name
                .child_by_field_name("alias")
                .or_else(|| name.named_child(0))
            {
                collect_targets(target, source, &mut self.shadowed);
            }
        }
    }

    fn owner(
        scopes: &[Self],
        here: usize,
        name: &str,
        position: usize,
        suite: Option<usize>,
    ) -> Callee {
        let mut enclosing = Some(here);
        while let Some(id) = enclosing {
            let scope = &scopes[id];
            if let Some(&binding) = scope.aliases.get(name) {
                if scope.shadowed.contains(name)
                    || (binding.conditional_suite.is_some()
                        && (id != here || binding.conditional_suite != suite))
                    || (id == here && binding.after > position)
                {
                    return Callee::Unresolved;
                }
                return Callee::Imported(binding);
            }
            if scope.shadowed.contains(name) {
                return Callee::Other;
            }
            enclosing = scope.parent;
        }
        Callee::Other
    }
}

pub(in crate::engine::impact) fn visit_calls<'tree>(
    root: Node<'tree>,
    source: &[u8],
    mut resolve: impl FnMut(&str, &str) -> Option<(u32, u32)>,
    mut emit: impl FnMut(Node<'tree>, u32, u32),
) -> HashSet<(u32, u32)> {
    if root.has_error() || memchr::memmem::find(source, b"as").is_none() {
        return HashSet::new();
    }
    let imports = imported_aliases(root, source, &mut resolve);
    if imports.is_empty() {
        return HashSet::new();
    }
    let local_names: HashSet<_> = imports.values().map(|(name, _)| *name).collect();
    let mut scopes = Vec::new();
    let mut unresolved = HashSet::new();
    let mut pending = vec![(root, None)];
    while let Some((declaration, parent)) = pending.pop() {
        let index = scopes.len();
        let mut scope = Scope {
            parent,
            ..Scope::default()
        };
        if let Some(parameters) = declaration.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for target in parameters
                .named_children(&mut cursor)
                .filter_map(parameter_target)
            {
                collect_targets(target, source, &mut scope.shadowed);
            }
        }
        let comprehension = is_comprehension(declaration.kind());
        let first_iterable = |node: Node<'tree>| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "for_in_clause")?
                .child_by_field_name("right")
        };
        let mut cursor = declaration.walk();
        let (mut parts, outside) = if comprehension {
            (
                declaration.named_children(&mut cursor).collect::<Vec<_>>(),
                first_iterable(declaration),
            )
        } else {
            (
                vec![
                    declaration
                        .child_by_field_name("body")
                        .unwrap_or(declaration),
                ],
                None,
            )
        };
        let mut calls = Vec::new();
        while let Some(part) = parts.pop() {
            let mut blocks = Vec::new();
            let _ = walk_preorder(part, Children::Named, |node, depth| {
                while blocks.last().is_some_and(|&(start, _)| start >= depth) {
                    blocks.pop();
                }
                if node.kind() == "block" {
                    blocks.push((depth, node.id()));
                }
                let suite = blocks.last().map(|&(_, id)| id);
                if Some(node) == outside {
                    return ControlFlow::Continue(Walk::Skip);
                }
                match node.kind() {
                    "function_definition" | "class_definition" | "lambda" => {
                        if let Some(name) = node.child_by_field_name("name") {
                            collect_targets(name, source, &mut scope.shadowed);
                        }
                        let body = node.child_by_field_name("body");
                        let mut cursor = node.walk();
                        // 関数・lambda の既定値、class の基底などは外側で評価される。
                        parts.extend(
                            node.named_children(&mut cursor)
                                .filter(|n| Some(*n) != body),
                        );
                    }
                    kind if is_comprehension(kind) => {
                        // 内包表記で先頭の iterable だけが外側で評価される。
                        parts.extend(first_iterable(node));
                    }
                    "import_from_statement" | "import_statement" => {
                        scope.record_import(node, source, suite.filter(|_| depth > 1), &imports);
                        return ControlFlow::Continue(Walk::Skip);
                    }
                    kind => {
                        let target = match kind {
                            "assignment"
                            | "augmented_assignment"
                            | "for_statement"
                            | "for_in_clause" => node.child_by_field_name("left"),
                            "as_pattern" => node.child_by_field_name("alias"),
                            "named_expression" => node.child_by_field_name("name"),
                            _ => None,
                        };
                        if let Some(target) = target {
                            collect_targets(target, source, &mut scope.shadowed);
                        }
                        if kind == "call"
                            && let Some(callee) = direct_callee(node)
                            && let Ok(name) = callee.utf8_text(source)
                            && local_names.contains(name)
                        {
                            calls.push((callee, name, suite));
                        }
                        return ControlFlow::<(), _>::Continue(Walk::Descend);
                    }
                }
                // メソッドや内包表記は class の名前空間を閉じ込めない。
                let outer = if declaration.kind() == "class_definition" {
                    parent
                } else {
                    Some(index)
                };
                pending.push((node, outer));
                ControlFlow::Continue(Walk::Skip)
            });
        }
        scopes.push(scope);
        for (callee, name, suite) in calls {
            match Scope::owner(&scopes, index, name, callee.start_byte(), suite) {
                Callee::Imported(binding) => emit(callee, binding.owner, binding.symbol),
                Callee::Unresolved => {
                    unresolved.insert(name);
                }
                Callee::Other => {}
            }
        }
    }
    multiline_import_sites(root, source, &imports, unresolved)
}
