//! 型注釈の配列・タプルから、対象の型の値を作る位置をたどる。

use super::*;

#[derive(Clone, Copy)]
enum Projection {
    Elements,
    Tuple { index: usize, len: usize },
}

struct ArrayVariable<'t> {
    declaration: Node<'t>,
    projections: Vec<Projection>,
    builtins: Vec<Node<'t>>,
}

fn array_variable<'t>(target: Node<'t>, source: &[u8]) -> Option<ArrayVariable<'t>> {
    let mut current = target;
    let mut projections = Vec::new();
    let mut builtins = Vec::new();
    let mut arrays = 0;
    for _ in 0..MAX_UNWRAP {
        let parent = current.parent()?;
        match parent.kind() {
            "array_type" if only_named_child(parent)?.id() == current.id() => {
                projections.push(Projection::Elements);
                arrays += 1;
                current = parent;
            }
            "parenthesized_type" if only_named_child(parent)?.id() == current.id() => {
                current = parent
            }
            "readonly_type"
                if matches!(current.kind(), "array_type" | "tuple_type")
                    && only_named_child(parent)?.id() == current.id() =>
            {
                current = parent
            }
            "tuple_type" => {
                let mut cursor = parent.walk();
                let mut len = 0;
                let mut index = None;
                for child in parent
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment")
                {
                    if matches!(
                        child.kind(),
                        "optional_type" | "rest_type" | "required_parameter" | "optional_parameter"
                    ) {
                        return None;
                    }
                    if child.id() == current.id() {
                        index = Some(len);
                    }
                    len += 1;
                }
                projections.push(Projection::Tuple { index: index?, len });
                current = parent;
            }
            "type_arguments" if only_named_child(parent)?.id() == current.id() => {
                let generic = parent
                    .parent()
                    .filter(|node| node.kind() == "generic_type")?;
                let name = generic.child_by_field_name("name")?;
                if !matches!(name.utf8_text(source).ok()?, "Array" | "ReadonlyArray") {
                    return None;
                }
                builtins.push(name);
                projections.push(Projection::Elements);
                arrays += 1;
                current = generic;
            }
            "type_annotation" if only_named_child(parent)?.id() == current.id() => {
                projections.reverse();
                if !matches!(projections.first(), Some(Projection::Elements)) {
                    return None;
                }
                let declaration = parent.parent().filter(|node| {
                    node.kind() == "variable_declarator"
                        && node
                            .child_by_field_name("type")
                            .is_some_and(|ty| ty.id() == parent.id())
                })?;
                return Some(ArrayVariable {
                    declaration,
                    projections,
                    builtins,
                });
            }
            _ => return None,
        }
        if arrays > MAX_ARRAY_DEPTH {
            return None;
        }
    }
    None
}

/// 穴・spread を除いた literal の要素を、全件展開する中間 Vec を作らずに列挙する。
fn visit_elements<'t>(array: Node<'t>, mut visit: impl FnMut(usize, Node<'t>)) -> Option<usize> {
    if array.kind() != "array" {
        return None;
    }
    let mut count = 0;
    let mut expect_element = true;
    let mut cursor = array.walk();
    for child in array.children(&mut cursor) {
        match child.kind() {
            "[" | "]" | "comment" => {}
            "," => {
                if expect_element {
                    return None;
                }
                expect_element = true;
            }
            "spread_element" => return None,
            _ => {
                if !expect_element {
                    return None;
                }
                visit(count, child);
                count += 1;
                expect_element = false;
            }
        }
    }
    Some(count)
}

impl Prover<'_, '_> {
    pub(super) fn prove_array_variable(
        &mut self,
        target: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> Option<bool> {
        if !matches!(self.delta.mode, MemberChangeMode::Addition) {
            return None;
        }
        let shape = array_variable(target, &parsed.source)?;
        Some(self.prove_array_variable_shape(shape, parsed, path, diff_files, caches))
    }

    fn prove_array_variable_shape(
        &mut self,
        shape: ArrayVariable<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !super::super::ts_const_arg::declarator_is_const(shape.declaration)
            || !local_producers::prove_array_variable(shape.declaration, parsed)
            || shape.builtins.iter().any(|node| {
                resolve_binding_scope(
                    *node,
                    &parsed.source,
                    node.utf8_text(&parsed.source).unwrap_or_default(),
                    Namespace::Type,
                )
                .is_some()
            })
        {
            return false;
        }
        if !shape.builtins.is_empty()
            && !*self.array_literal_builtins.get_or_init(|| {
                let names = BTreeSet::from(["Array", "ReadonlyArray"]);
                ensure_name_refs(self.scope.dir, &names, caches);
                self.builtin_types_are_plain(&names, caches)
            })
        {
            return false;
        }
        let Some(value) = shape.declaration.child_by_field_name("value") else {
            return false;
        };
        let mut pending = vec![(value, 0)];
        while let Some((value, index)) = pending.pop() {
            let Some(value) = unwrap_parens(value) else {
                return false;
            };
            let Some(projection) = shape.projections.get(index) else {
                if !self.prove_value(value, parsed, path, diff_files, caches) {
                    return false;
                }
                continue;
            };
            match *projection {
                Projection::Elements => {
                    if visit_elements(value, |_, child| pending.push((child, index + 1))).is_none()
                    {
                        return false;
                    }
                }
                Projection::Tuple {
                    index: position,
                    len,
                } => {
                    let mut selected = None;
                    if visit_elements(value, |at, child| {
                        if at == position {
                            selected = Some(child);
                        }
                    }) != Some(len)
                    {
                        return false;
                    }
                    let Some(selected) = selected else {
                        return false;
                    };
                    pending.push((selected, index + 1));
                }
            }
        }
        self.sites += 1;
        true
    }
}
