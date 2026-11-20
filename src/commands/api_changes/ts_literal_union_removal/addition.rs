//! 文字列 union へ要素を足したときの、限定した生成元と網羅的な読み手の証明。

use super::*;
use slots::Primitive;

pub(super) fn audit(
    inventory: &inventory::Inventory,
    site: &CompatibleModSite<'_>,
    values: &BTreeSet<String>,
) -> Option<()> {
    let plan = slots::collect_addition(inventory, site, values)?;
    producers_are_unreferenced(inventory, &plan.producers)?;
    flows::audit_addition(inventory, &plan, values)
}

fn array_alias(annotation: Node<'_>) -> Option<Node<'_>> {
    if annotation.kind() != "type_annotation" {
        return None;
    }
    let array = only(annotation).filter(|node| node.kind() == "array_type")?;
    only(array).filter(|node| node.kind() == "type_identifier")
}

pub(super) fn array_producer(
    inventory: &inventory::Inventory,
    path: &str,
    alias: Node<'_>,
    target: &Site,
    values: &BTreeSet<String>,
) -> Option<Site> {
    let annotation = alias.parent()?.parent()?;
    let declaration = annotation.parent()?;
    let function = match declaration.kind() {
        "function_declaration" => declaration,
        "variable_declarator" => declaration.parent()?.parent()?.parent()?,
        _ => return None,
    };
    if function.kind() != "function_declaration"
        || function.child_by_field_name("type_parameters").is_some()
        || token(function, "async")
        || token(function.parent()?, "default")
    {
        return None;
    }
    let site = slots::declared_site(inventory, path, function)?;
    let source = inventory.source(path)?;
    let returns = array_alias(function.child_by_field_name("return_type")?)?;
    if text(returns, source) != Some(&target.name)
        || slots::type_owner(inventory, path, returns, &target.name)? != *target
    {
        return None;
    }

    let mut parameters = BTreeMap::new();
    for parameter in children(function.child_by_field_name("parameters")?) {
        let name = parameter.child_by_field_name("pattern")?;
        let ty = Primitive::read(only(parameter.child_by_field_name("type")?)?, source)?;
        if parameter.kind() != "required_parameter"
            || parameter.child_by_field_name("value").is_some()
            || name.kind() != "identifier"
            || !plain_name(text(name, source)?)
            || ty == Primitive::Void
            || parameters.insert(text(name, source)?, ty).is_some()
        {
            return None;
        }
    }

    let body = children(function.child_by_field_name("body")?);
    let (last, statements) = body.split_last()?;
    let returned = only(*last).filter(|_| last.kind() == "return_statement")?;
    if statements.is_empty() {
        slots::literal_array(returned, source, values)?;
        return (alias == returns).then_some(site);
    }
    let (initial, appends) = statements.split_first()?;
    let local = only(*initial).filter(|node| node.kind() == "variable_declarator")?;
    let name = local.child_by_field_name("name")?;
    let local_type = array_alias(local.child_by_field_name("type")?)?;
    if name.kind() != "identifier"
        || !plain_name(text(name, source)?)
        || parameters.contains_key(text(name, source)?)
        || !slots::is_const(local)
        || returned.kind() != "identifier"
        || text(returned, source) != text(name, source)
        || (alias != returns && alias != local_type)
        || text(local_type, source) != Some(&target.name)
        || slots::type_owner(inventory, path, local_type, &target.name)? != *target
    {
        return None;
    }
    slots::literal_array(local.child_by_field_name("value")?, source, values)?;
    for statement in appends {
        append(*statement, text(name, source)?, &parameters, source, values)?;
    }
    Some(site)
}

fn append(
    mut statement: Node<'_>,
    array: &str,
    parameters: &BTreeMap<&str, Primitive>,
    source: &str,
    values: &BTreeSet<String>,
) -> Option<()> {
    if statement.kind() == "if_statement" {
        let test = unparen(statement.child_by_field_name("condition")?)?;
        let left = test.child_by_field_name("left")?;
        let right = test.child_by_field_name("right")?;
        let op = test.child_by_field_name("operator")?;
        if statement.child_by_field_name("alternative").is_some()
            || test.kind() != "binary_expression"
            || left.kind() != "identifier"
            || !matches!(op.kind(), "===" | "!==" | "<" | "<=" | ">" | ">=")
            || !parameters.get(text(left, source)?)?.accepts(right, source)
        {
            return None;
        }
        statement = statement.child_by_field_name("consequence")?;
    }
    let call = only(statement)
        .filter(|call| statement.kind() == "expression_statement" && ordinary_call(*call))?;
    let member = call.child_by_field_name("function")?;
    let receiver = member.child_by_field_name("object")?;
    if member.kind() != "member_expression"
        || member.child_by_field_name("optional_chain").is_some()
        || token(member, "?.")
        || receiver.kind() != "identifier"
        || text(receiver, source) != Some(array)
        || text(member.child_by_field_name("property")?, source) != Some("push")
    {
        return None;
    }
    let arguments = children(call.child_by_field_name("arguments")?);
    (!arguments.is_empty()
        && arguments
            .iter()
            .all(|node| decode::string(*node, source).is_some_and(|value| values.contains(&value))))
    .then_some(())
}

fn producers_are_unreferenced(inventory: &inventory::Inventory, sites: &[Site]) -> Option<()> {
    if sites.is_empty() {
        return Some(());
    }
    let names = sites.iter().map(|site| site.name.clone()).collect();
    for (path, file) in inventory.relevant(&names) {
        for node in slots::nodes(inventory.root(path)?) {
            if !crate::engine::js_occurrence::identifier_kind(node.kind()) {
                continue;
            }
            let raw = text(node, &file.source)?;
            if names.contains(raw)
                && !sites.iter().any(|site| {
                    site.path == path
                        && site.name == raw
                        && node.parent().is_some_and(|parent| {
                            span(parent) == site.span
                                && parent.child_by_field_name("name") == Some(node)
                        })
                })
            {
                // 戻り値の型は、別名への明示的な参照がない呼び出しにも伝わる。
                return None;
            }
        }
    }
    Some(())
}

pub(super) fn exhaustive_switch(
    switch: Node<'_>,
    source: &str,
    values: &BTreeSet<String>,
) -> Option<()> {
    let name = unparen(switch.child_by_field_name("value")?)?;
    let block = switch
        .parent()
        .filter(|node| node.kind() == "statement_block")?;
    let function = block
        .parent()
        .filter(|node| matches!(node.kind(), "function_declaration" | "arrow_function"))?;
    if name.kind() != "identifier" || function.child_by_field_name("body") != Some(block) {
        return None;
    }
    // 判別するのは、switch を本体の直下に持つ関数自身の引数に限る。
    let (scope, binding) = resolve_binding_scope(
        name,
        source.as_bytes(),
        text(name, source)?,
        Namespace::Value,
    )?;
    if scope.id() != function.id() || !matches!(binding, ScopeBinding::Other) {
        return None;
    }
    let result = Primitive::read(only(function.child_by_field_name("return_type")?)?, source)?;
    let mut covered = BTreeSet::new();
    let mut default_seen = false;
    for arm in children(switch.child_by_field_name("body")?) {
        if arm.kind() == "switch_default" {
            let witness = never_value(arm, source)?;
            if default_seen || text(witness, source) != text(name, source) {
                return None;
            }
            default_seen = true;
            continue;
        }
        let statements = children(arm);
        let [literal, returned] = statements.as_slice() else {
            return None;
        };
        let returns = match children(*returned).as_slice() {
            [] => result == Primitive::Void,
            [value] => result.accepts(*value, source),
            _ => false,
        };
        if arm.kind() != "switch_case"
            || arm.child_by_field_name("value") != Some(*literal)
            || literal.kind() != "string"
            || returned.kind() != "return_statement"
            || !covered.insert(decode::string(*literal, source)?)
            || !returns
        {
            return None;
        }
    }
    (covered == *values).then_some(())
}

fn never_value<'t>(default: Node<'t>, source: &str) -> Option<Node<'t>> {
    let body = children(default);
    let block = match body.as_slice() {
        [block] if block.kind() == "statement_block" => children(*block),
        _ => body,
    };
    let [declaration, returned] = block.as_slice() else {
        return None;
    };
    let variable = only(*declaration).filter(|node| node.kind() == "variable_declarator")?;
    let ty = only(variable.child_by_field_name("type")?)?;
    let lhs = variable.child_by_field_name("name")?;
    let rhs = variable.child_by_field_name("value")?;
    let returned_value = only(*returned)?;
    (slots::is_const(variable)
        && text(ty, source) == Some("never")
        && lhs.kind() == "identifier"
        && rhs.kind() == "identifier"
        && returned.kind() == "return_statement"
        && returned_value.kind() == "identifier"
        && text(lhs, source) == text(returned_value, source)
        && text(lhs, source) != text(rhs, source))
    .then_some(rhs)
}

pub(super) fn proves_never_use(
    node: Node<'_>,
    source: &str,
    values: &BTreeSet<String>,
) -> Option<()> {
    let block = node.parent()?.parent()?.parent()?;
    let default = if block.kind() == "statement_block" {
        block.parent()?
    } else {
        block
    };
    if default.kind() != "switch_default" || never_value(default, source)? != node {
        return None;
    }
    let switch = default.parent()?.parent()?;
    exhaustive_switch(switch, source, values)
}
