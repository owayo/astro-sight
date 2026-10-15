//! 型の証明と分けて、数値利用などを示す限定された使用位置を記録する。

use tree_sitter::Node;

use super::{PythonFile, children, enclosing_function, nodes, parameter_annotation, text};
use crate::models::review::MemberTypeDemand;

pub(super) fn ordinary_method(file: &PythonFile, class: Node<'_>, member: &str) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };
    let mut found = false;
    for child in children(body) {
        let definition = if child.kind() == "decorated_definition" {
            child.child_by_field_name("definition")
        } else {
            Some(child)
        };
        let Some(definition) = definition.filter(|node| node.kind() == "function_definition")
        else {
            continue;
        };
        if definition
            .child_by_field_name("name")
            .and_then(|name| text(name, &file.source))
            != Some(member)
        {
            continue;
        }
        if found
            || child.kind() != "function_definition"
            || definition
                .child(0)
                .is_some_and(|node| node.kind() == "async")
        {
            return false;
        }
        if !file.bound_once(member, definition.id(), Some(class)) {
            return false;
        }
        found = true;
    }
    found
        && !file.deleted_names.contains(member)
        && !nodes(file.tree.root_node()).iter().any(|node| {
            node.kind() == "attribute"
                && node
                    .child_by_field_name("attribute")
                    .and_then(|name| text(name, &file.source))
                    == Some(member)
                && !super::super::removed_scope::python_is_load(*node)
        })
}

pub(super) fn unknown_receiver<'a>(
    file: &PythonFile,
    receiver: Node<'a>,
    owner: &str,
    mut typed_field: impl FnMut(Node<'a>, &str) -> bool,
) -> bool {
    if receiver.kind() != "identifier" {
        return false;
    }
    let Some(name) = text(receiver, &file.source) else {
        return false;
    };
    if matches!(name, "self" | "cls")
        || name == owner
        || file
            .bindings
            .get(name)
            .is_some_and(|bindings| bindings.iter().any(|binding| binding.imported))
        || file.deleted_names.contains(name)
    {
        return false;
    }
    let Some(function) = enclosing_function(receiver) else {
        return false;
    };
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    if receiver.start_byte() < body.start_byte()
        || receiver.end_byte() > body.end_byte()
        || function
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition")
        || parameter_annotation(file, function, name).is_some()
    {
        return false;
    }
    let mut ancestor = receiver.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "try_statement" | "with_statement" | "lambda") {
            return false;
        }
        if node.kind() == "generator_expression" && !immediate_sum(file, node) {
            return false;
        }
        if node.id() == function.id() {
            break;
        }
        ancestor = node.parent();
    }
    // class method の第1引数は、綴りが self / cls 以外でも保留する。
    let parameters = function.child_by_field_name("parameters");
    if function
        .parent()
        .and_then(|body| body.parent())
        .is_some_and(|class| class.kind() == "class_definition")
        && parameters
            .and_then(|parameters| parameters.named_child(0))
            .is_some_and(|parameter| {
                text(parameter, &file.source) == Some(name)
                    || parameter
                        .named_child(0)
                        .and_then(|node| text(node, &file.source))
                        == Some(name)
            })
    {
        return false;
    }
    let mut ancestor = receiver.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "generator_expression" | "list_comprehension") {
            let clauses: Vec<_> = children(node)
                .into_iter()
                .filter(|child| child.kind() == "for_in_clause")
                .collect();
            let [clause] = clauses.as_slice() else {
                return false;
            };
            let Some(left) = clause.child_by_field_name("left") else {
                return false;
            };
            let Some(iterable) = clause.child_by_field_name("right") else {
                return false;
            };
            if !unknown_iterable(file, function, iterable, &mut typed_field) {
                return false;
            }
            let in_body = node.child_by_field_name("body").is_some_and(|body| {
                body.start_byte() <= receiver.start_byte() && receiver.end_byte() <= body.end_byte()
            });
            let in_filter = children(node).iter().any(|child| {
                child.kind() == "if_clause"
                    && child.start_byte() <= receiver.start_byte()
                    && receiver.end_byte() <= child.end_byte()
            });
            return left.kind() == "identifier"
                && text(left, &file.source) == Some(name)
                && (in_body || in_filter)
                && file.bound_once(name, clause.id(), Some(node));
        }
        if matches!(
            node.kind(),
            "set_comprehension" | "dictionary_comprehension"
        ) {
            return false;
        }
        if node.id() == function.id() {
            break;
        }
        ancestor = node.parent();
    }
    parameters.is_some_and(|parameters| {
        children(parameters).iter().any(|parameter| {
            parameter.kind() == "identifier"
                && text(*parameter, &file.source) == Some(name)
                && file.bound_once(name, parameter.id(), Some(function))
        })
    })
}

fn unknown_iterable<'a>(
    file: &PythonFile,
    function: Node<'a>,
    mut node: Node<'a>,
    typed_field: &mut impl FnMut(Node<'a>, &str) -> bool,
) -> bool {
    let mut first_field = None;
    for _ in 0..128 {
        match node.kind() {
            "parenthesized_expression" if node.named_child_count() == 1 => {
                node = node.named_child(0).unwrap();
            }
            "attribute" => {
                let Some(object) = node.child_by_field_name("object") else {
                    return false;
                };
                first_field = node
                    .child_by_field_name("attribute")
                    .and_then(|name| text(name, &file.source));
                node = object;
            }
            "identifier" => {
                let Some(name) = text(node, &file.source) else {
                    return false;
                };
                if file.deleted_names.contains(name) {
                    return false;
                }
                return function
                    .child_by_field_name("parameters")
                    .is_some_and(|parameters| {
                        children(parameters).iter().any(|parameter| {
                            let parameter_name = match parameter.kind() {
                                "identifier" => Some(*parameter),
                                "typed_parameter" if first_field.is_some() => parameter
                                    .child_by_field_name("name")
                                    .or_else(|| parameter.named_child(0)),
                                _ => None,
                            };
                            parameter_name.and_then(|parameter| text(parameter, &file.source))
                                == Some(name)
                                && file.bound_once(name, parameter.id(), Some(function))
                                && (parameter.kind() == "identifier"
                                    || first_field.is_some_and(|field| typed_field(node, field)))
                        })
                    });
            }
            _ => return false,
        }
    }
    false
}

fn immediate_sum(file: &PythonFile, mut generator: Node<'_>) -> bool {
    for _ in 0..128 {
        let Some(parent) = generator.parent() else {
            return false;
        };
        if parent.kind() == "parenthesized_expression" && parent.named_child_count() == 1 {
            generator = parent;
            continue;
        }
        let call = if parent.kind() == "argument_list" {
            parent.parent()
        } else {
            Some(parent)
        };
        return call.is_some_and(|call| {
            call_demand(file, call, generator, true) == Some(MemberTypeDemand::SumElement)
        });
    }
    false
}

pub(super) fn value_demand(file: &PythonFile, mut value: Node<'_>) -> Option<MemberTypeDemand> {
    let mut element = false;
    loop {
        let parent = value.parent()?;
        match parent.kind() {
            "parenthesized_expression" if parent.named_child_count() == 1 => value = parent,
            "conditional_expression" if !element => {
                let parts = children(parent);
                if parts.len() != 3 || parts[1].id() == value.id() {
                    return None;
                }
                value = parent;
            }
            "generator_expression" | "list_comprehension" if !element => {
                if parent.child_by_field_name("body")?.id() != value.id() {
                    return None;
                }
                element = true;
                value = parent;
            }
            "unary_operator" if !element => {
                let argument = parent.child_by_field_name("argument")?;
                let operator =
                    std::str::from_utf8(&file.source[parent.start_byte()..argument.start_byte()])
                        .ok()?
                        .trim();
                return (argument.id() == value.id() && matches!(operator, "+" | "-" | "~"))
                    .then_some(MemberTypeDemand::UnaryNumeric);
            }
            "binary_operator" if !element => {
                let left = parent.child_by_field_name("left")?;
                let right = parent.child_by_field_name("right")?;
                let other = if left.id() == value.id() {
                    right
                } else if right.id() == value.id() {
                    left
                } else {
                    return None;
                };
                let operator =
                    std::str::from_utf8(&file.source[left.end_byte()..right.start_byte()])
                        .ok()?
                        .trim();
                return (numeric_literal(file, other)
                    && matches!(
                        operator,
                        "+" | "-" | "*" | "**" | "/" | "//" | "<<" | ">>" | "&" | "|" | "^"
                    ))
                .then_some(MemberTypeDemand::NumericArithmetic);
            }
            "comparison_operator" if !element => {
                let operands = children(parent);
                let [left, right] = operands.as_slice() else {
                    return None;
                };
                let other = if left.id() == value.id() {
                    *right
                } else if right.id() == value.id() {
                    *left
                } else {
                    return None;
                };
                let operator =
                    std::str::from_utf8(&file.source[left.end_byte()..right.start_byte()])
                        .ok()?
                        .trim();
                return (numeric_literal(file, other)
                    && matches!(operator, "<" | "<=" | ">" | ">="))
                .then_some(MemberTypeDemand::NumericOrdering);
            }
            "argument_list" => return call_demand(file, parent.parent()?, value, element),
            "call" => return call_demand(file, parent, value, element),
            _ => return None,
        }
    }
}

fn call_demand(
    file: &PythonFile,
    call: Node<'_>,
    value: Node<'_>,
    element: bool,
) -> Option<MemberTypeDemand> {
    if call.kind() != "call" {
        return None;
    }
    let callee = call.child_by_field_name("function")?;
    if callee.kind() != "identifier" {
        return None;
    }
    let name = text(callee, &file.source)?;
    if file.bindings.contains_key(name) || file.deleted_names.contains(name) {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let args = if arguments.kind() == "argument_list" {
        children(arguments)
    } else {
        vec![arguments]
    };
    if args.first()?.id() != value.id() {
        return None;
    }
    if element {
        return (name == "sum"
            && (args.len() == 1 || (args.len() == 2 && numeric_literal(file, args[1]))))
        .then_some(MemberTypeDemand::SumElement);
    }
    if args.len() != 1 {
        return None;
    }
    match name {
        "int" => Some(MemberTypeDemand::Int),
        "float" => Some(MemberTypeDemand::Float),
        "abs" => Some(MemberTypeDemand::Abs),
        "round" => Some(MemberTypeDemand::Round),
        "range" => Some(MemberTypeDemand::Range),
        "len" => Some(MemberTypeDemand::Len),
        _ => None,
    }
}

fn numeric_literal(file: &PythonFile, mut node: Node<'_>) -> bool {
    for _ in 0..128 {
        match node.kind() {
            "integer" | "float" => {
                return text(node, &file.source).is_some_and(|text| !text.ends_with(['j', 'J']));
            }
            "parenthesized_expression" if node.named_child_count() == 1 => {
                node = node.named_child(0).unwrap();
            }
            "unary_operator" => {
                let Some(argument) = node.child_by_field_name("argument") else {
                    return false;
                };
                if !std::str::from_utf8(&file.source[node.start_byte()..argument.start_byte()])
                    .ok()
                    .is_some_and(|operator| matches!(operator.trim(), "+" | "-"))
                {
                    return false;
                }
                node = argument;
            }
            _ => return false,
        }
    }
    false
}
