//! 必須追加では、型を証明した値からのコピーと後続の上書きを順番に照合する。

use super::*;

const COPY_RETURN_BOUNDARIES: &[&str] = &[
    "function_declaration",
    "function_expression",
    "generator_function_declaration",
    "generator_function",
    "arrow_function",
    "method_definition",
    "class_body",
];

#[derive(Clone, Copy)]
struct ExplicitValue<'t> {
    value: Node<'t>,
    line: usize,
    order: usize,
}

impl Prover<'_, '_> {
    pub(super) fn prove_spread_object(
        &self,
        object: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let source: &[u8] = &parsed.source;
        let names: HashMap<&str, usize> = self
            .delta
            .added
            .iter()
            .enumerate()
            .map(|(index, property)| (property.name.as_str(), index))
            .collect();
        let mut explicit: Vec<Option<ExplicitValue<'_>>> = vec![None; self.delta.added.len()];
        let mut last_spread = None;
        let mut cursor = object.walk();
        for (order, member) in object.named_children(&mut cursor).enumerate() {
            if member.kind() == "comment" {
                continue;
            }
            if member.kind() == "spread_element" {
                let Some(value) = only_named_child(member).and_then(unwrap_parens) else {
                    return false;
                };
                let proven = match value.kind() {
                    "identifier" => self.identifier_has_target_type(value, parsed, path, caches),
                    "call_expression" => self.call_returns_target_for(
                        value,
                        parsed,
                        path,
                        CallResultUse::ObjectSpread,
                        caches,
                    ),
                    _ => false,
                };
                if !proven {
                    return false;
                }
                last_spread = Some(order);
                continue;
            }
            let (key, value) = match member.kind() {
                "pair" => match (
                    member.child_by_field_name("key"),
                    member.child_by_field_name("value"),
                ) {
                    (Some(key), Some(value)) => (key, value),
                    _ => return false,
                },
                "shorthand_property_identifier" => (member, member),
                _ => return false,
            };
            if key.kind() == "computed_property_name"
                || key.utf8_text(source).is_ok_and(|text| text.contains('\\'))
            {
                return false;
            }
            let Some(key) = super::super::object_key_text(key, source) else {
                return false;
            };
            if let Some(&index) = names.get(key.as_str()) {
                if explicit[index].is_some() {
                    return false;
                }
                explicit[index] = Some(ExplicitValue {
                    value,
                    line: member.start_position().row,
                    order,
                });
            }
        }
        let Some(last_spread) = last_spread else {
            return false;
        };
        // spread ごとに全プロパティを展開せず、最後の位置と明示値の位置だけで比べる。
        let needs_lines = explicit
            .iter()
            .flatten()
            .any(|value| value.order > last_spread);
        let changed = if needs_lines {
            let Some(changed) = changed_lines_for_ref_path(
                self.scope.dir,
                self.scope.base,
                path,
                diff_files,
                caches,
            ) else {
                return false;
            };
            Some(changed)
        } else {
            None
        };
        self.delta
            .added
            .iter()
            .enumerate()
            .all(|(index, property)| {
                let Some(value) = explicit[index].filter(|value| value.order > last_spread) else {
                    return true;
                };
                changed
                    .as_ref()
                    .is_some_and(|lines| lines.contains(&value.line))
                    && (property.accepts_any_value()
                        || self
                            .value_type(value.value, parsed, 0)
                            .is_some_and(|ty| assignable(&ty, &property.ty)))
            })
    }
}

/// コピー先の注釈は既存の型参照の義務で所属と値を別に証明し、束縛名も監査する。
/// ここではその義務を持つ構文だけを識別する。無注釈の入れ物は例外にしない。
pub(super) fn is_typed_copy_source(node: Node<'_>, source: &[u8], type_name: &str) -> bool {
    let Some((value, spread)) = wrapped_parent(node) else {
        return false;
    };
    if spread.kind() != "spread_element"
        || only_named_child(spread).is_none_or(|inner| inner.id() != value.id())
    {
        return false;
    }
    let Some(object) = spread.parent().filter(|parent| parent.kind() == "object") else {
        return false;
    };
    let Some((value, destination)) = wrapped_parent(object) else {
        return false;
    };
    let target = |annotation: Node<'_>| {
        only_named_child(annotation).is_some_and(|ty| {
            ty.kind() == "type_identifier" && ty.utf8_text(source).ok() == Some(type_name)
        })
    };
    match destination.kind() {
        "variable_declarator" => {
            destination
                .child_by_field_name("value")
                .is_some_and(|init| init.id() == value.id())
                && super::super::ts_const_arg::declarator_is_const(destination)
                && destination
                    .child_by_field_name("name")
                    .is_some_and(|name| name.kind() == "identifier")
                && destination.child_by_field_name("type").is_some_and(target)
        }
        "satisfies_expression" => {
            let mut cursor = destination.walk();
            let parts: Vec<_> = destination
                .named_children(&mut cursor)
                .filter(|node| node.kind() != "comment")
                .collect();
            matches!(parts.as_slice(), [expression, ty] if expression.id() == value.id() && ty.kind() == "type_identifier" && ty.utf8_text(source).ok() == Some(type_name))
        }
        "return_statement"
            if only_named_child(destination).is_some_and(|ret| ret.id() == value.id()) =>
        {
            let mut parent = destination.parent();
            while let Some(function) = parent {
                // 最も近い callable で止める。参照ごとの関数本体の再走査を避ける。
                if COPY_RETURN_BOUNDARIES.contains(&function.kind()) {
                    return function.kind() == "function_declaration"
                        && is_top_level_declaration(function)
                        && function
                            .child_by_field_name("return_type")
                            .is_some_and(target);
                }
                parent = function.parent();
            }
            false
        }
        _ => false,
    }
}

fn wrapped_parent(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let parent = current.parent()?;
        if parent.kind() == "parenthesized_expression"
            && only_named_child(parent).is_some_and(|inner| inner.id() == current.id())
        {
            current = parent;
        } else {
            return Some((current, parent));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_destinations_use_real_ts_and_tsx_nodes() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in COPY_RETURN_BOUNDARIES {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @boundary"))
                    .unwrap();
            }
            for kind in [
                "object",
                "spread_element",
                "pair",
                "shorthand_property_identifier",
                "variable_declarator",
                "satisfies_expression",
                "return_statement",
                "parenthesized_expression",
                "function_declaration",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node")).unwrap();
            }
            for (source, expected) in [
                (
                    "export function f(row: Row): Row { return { ...(row), id: 'x' }; }",
                    true,
                ),
                ("const copy: Row = { ...row, id };", true),
                ("const copy = ({ ...row } satisfies Row);", true),
                ("const copy = { ...row };", false),
                ("function f(row: Row) { return { ...row }; }", false),
                ("function f(row: Row): Other { return { ...row }; }", false),
                (
                    "function f(row: Row): Row { const g = () => { return { ...row }; }; return row; }",
                    false,
                ),
                (
                    "const g = function(row: Row): Row { return { ...row }; };",
                    false,
                ),
                ("function* g(row: Row): Row { return { ...row }; }", false),
                (
                    "const g = function*(row: Row): Row { return { ...row }; };",
                    false,
                ),
                (
                    "const g = { copy(row: Row): Row { return { ...row }; } };",
                    false,
                ),
                (
                    "function f(row: Row): Row { class C { static { return { ...row }; } } return row; }",
                    false,
                ),
            ] {
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let mut results = Vec::new();
                let _ = walk_preorder(tree.root_node(), Children::Named, |node, _| {
                    if node.kind() == "spread_element" {
                        let operand = only_named_child(node).and_then(unwrap_parens).unwrap();
                        assert_eq!(operand.kind(), "identifier");
                        assert_eq!(operand.utf8_text(source.as_bytes()).unwrap(), "row");
                        results.push(is_typed_copy_source(operand, source.as_bytes(), "Row"));
                    }
                    ControlFlow::<(), Walk>::Continue(Walk::Descend)
                });
                assert_eq!(results, vec![expected], "{lang:?}/{source}");
            }
        }
    }
}
