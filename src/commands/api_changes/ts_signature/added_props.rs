//! inline object 引数へ追加したキーを、既存の呼び出しで渡すか、その値の型を証明する。

use super::pattern_extension::caller_literal_matches;
use super::removed_params::{
    OptionalRemovalCache, UseRule, object_properties_match, prove_repository_uses,
};
use super::{ApiRefIndex, CompatibleModSite, module_root, ts_union_type_parts};
use crate::commands::api_changes::ts_required_members::INHERITED_NAMES;
use crate::engine::jsx::{caller_shape, jsx_name_opening_tag};
use std::collections::HashMap;
use tree_sitter::Node;

mod return_contract;
pub(super) use return_contract::{preserves_return, returns_jsx};

#[derive(Default)]
pub(super) struct PropertyTypes<'tree> {
    pub(super) named: HashMap<String, Node<'tree>>,
    pub(super) unresolved: bool,
}

impl PropertyTypes<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.named.is_empty() && !self.unresolved
    }
}

pub(super) struct ParameterAddition<'a> {
    pub(super) position: usize,
    pub(super) parameter: Node<'a>,
    pub(super) members: PropertyTypes<'a>,
}

impl ParameterAddition<'_> {
    fn omittable(&self) -> bool {
        self.parameter.kind() == "optional_parameter"
            || self.parameter.child_by_field_name("value").is_some()
    }

    fn accepts(&self, value: Node<'_>, source: &[u8], types: &[u8]) -> bool {
        if value.kind() == "object" {
            return object_accepts(value, source, &self.members, types);
        }
        let Some(ty) = self
            .parameter
            .child_by_field_name("type")
            .and_then(|n| n.named_child(0))
        else {
            return false;
        };
        if unshadowed_undefined(value, source) {
            return self.omittable()
                || ts_union_type_parts(ty)
                    .iter()
                    .any(|part| part.utf8_text(types).ok() == Some("undefined"));
        }
        caller_literal_matches(Some(value), source, ty, types)
    }
}

pub(super) struct AddedPropUses<'a> {
    parameters: Vec<ParameterAddition<'a>>,
    type_source: &'a [u8],
    declarations: Vec<usize>,
    inner_name: Option<(&'a str, std::ops::Range<usize>)>,
    jsx_standard: bool,
    pub(super) audited_react: bool,
}

pub(super) fn repository_accepts(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    cache: &mut OptionalRemovalCache,
    function: Node<'_>,
    source: &[u8],
    mut additions: Vec<ParameterAddition<'_>>,
    audited_react: bool,
) -> bool {
    if site.name.contains('.') || site.old_path != site.new_path {
        return false;
    }
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return false;
    };
    let receiver = parameters
        .named_children(&mut parameters.walk())
        .find(|n| !n.is_extra())
        .and_then(|p| p.child_by_field_name("pattern"))
        .is_some_and(|p| p.utf8_text(source).ok() == Some("this"));
    for addition in &mut additions {
        if addition.members.unresolved
            || addition
                .members
                .named
                .keys()
                .any(|name| INHERITED_NAMES.contains(&name.as_str()))
        {
            return false;
        }
        if let Some(value) = addition.parameter.child_by_field_name("value")
            && !addition.accepts(value, source, source)
        {
            return false;
        }
        if receiver {
            let Some(index) = addition.position.checked_sub(1) else {
                return false;
            };
            addition.position = index;
        }
    }
    let Some(refs) = index.complete_refs_for(site.name) else {
        return false;
    };
    let Some((inventory, evidence)) = cache.inventory(site.dir, site.name, site.new_path) else {
        return false;
    };
    if !inventory.has_source(site.new_path, source) || evidence.declares_global("Object") {
        return false;
    }
    let mut declarations = Vec::new();
    let inner_name = function.child_by_field_name("name").and_then(|node| {
        let name = node.utf8_text(source).ok()?;
        (name != site.name && function.kind() == "function_expression").then(|| {
            declarations.push(node.start_byte());
            (name, function.byte_range())
        })
    });
    let mut current = Some(function);
    while let Some(node) = current {
        if matches!(
            node.kind(),
            "function_declaration" | "function_expression" | "variable_declarator"
        ) && let Some(name) = node.child_by_field_name("name")
            && name.utf8_text(source).ok() == Some(site.name)
        {
            declarations.push(name.start_byte());
        }
        current = node.parent();
    }
    if declarations.is_empty() {
        return false;
    }
    let uses = AddedPropUses {
        parameters: additions,
        type_source: source,
        declarations,
        inner_name,
        audited_react,
        jsx_standard: audited_react
            || evidence
                .repository
                .as_ref()
                .is_some_and(|audit| audit.jsx_standard()),
    };
    prove_repository_uses(
        refs,
        &inventory,
        evidence.relaxation(),
        site.new_path,
        site.name,
        UseRule::AddedProps(&uses),
    )
    .is_some()
}

impl AddedPropUses<'_> {
    pub(super) fn matches_inner_name(&self, name: &str, node: Node<'_>) -> bool {
        self.inner_name
            .as_ref()
            .is_some_and(|(inner, range)| *inner == name && range.contains(&node.start_byte()))
    }

    pub(super) fn is_declaration(&self, node: Node<'_>) -> bool {
        self.declarations.contains(&node.start_byte())
    }

    pub(super) fn accepts(&self, name: Node<'_>, source: &[u8]) -> bool {
        if let Some(open) = jsx_name_opening_tag(name, source) {
            return self.jsx_standard
                && self.parameters.iter().all(|param| {
                    param.position == 0 && self.jsx_accepts(name, open, source, &param.members)
                });
        }
        let Some(call) = name.parent().filter(|n| n.kind() == "call_expression") else {
            return false;
        };
        if call.child_by_field_name("function").map(|n| n.id()) != Some(name.id())
            || call.child_by_field_name("type_arguments").is_some()
        {
            return false;
        }
        let Some(args) = call
            .child_by_field_name("arguments")
            .filter(|n| n.kind() == "arguments")
        else {
            return false;
        };
        let args: Vec<_> = args
            .named_children(&mut args.walk())
            .filter(|n| !n.is_extra())
            .collect();
        !args.iter().any(|n| n.kind() == "spread_element")
            && self.parameters.iter().all(|param| {
                args.get(param.position).map_or_else(
                    || param.omittable(),
                    |arg| param.accepts(*arg, source, self.type_source),
                )
            })
    }

    fn jsx_accepts(
        &self,
        name: Node<'_>,
        open: Node<'_>,
        source: &[u8],
        members: &PropertyTypes<'_>,
    ) -> bool {
        let Some(shape) = caller_shape(name, source, true) else {
            return false;
        };
        if shape.children && members.named.contains_key("children") {
            return false;
        }
        open.named_children(&mut open.walk())
            .filter(|n| n.kind() == "jsx_attribute")
            .all(|attribute| {
                let Some(key) = attribute
                    .named_child(0)
                    .and_then(|n| n.utf8_text(source).ok())
                else {
                    return false;
                };
                let Some(ty) = members.named.get(key) else {
                    return true;
                };
                let value = match attribute.named_child(1) {
                    Some(expression) if expression.kind() == "jsx_expression" => {
                        let mut cursor = expression.walk();
                        let mut children = expression
                            .named_children(&mut cursor)
                            .filter(|n| !n.is_extra());
                        let (Some(value), None) = (children.next(), children.next()) else {
                            return false;
                        };
                        Some(value)
                    }
                    Some(string)
                        if string.kind() == "string"
                            && string
                                .utf8_text(source)
                                .is_ok_and(|s| !s.contains(['&', '\\', '\n', '\r'])) =>
                    {
                        Some(string)
                    }
                    Some(_) => return false,
                    None => None,
                };
                caller_literal_matches(value, source, *ty, self.type_source)
            })
    }
}

fn object_accepts(
    object: Node<'_>,
    source: &[u8],
    members: &PropertyTypes<'_>,
    type_source: &[u8],
) -> bool {
    object_properties_match(object, source, |key, entry| {
        members.named.get(key).is_none_or(|ty| {
            entry
                .child_by_field_name("value")
                .is_some_and(|value| caller_literal_matches(Some(value), source, *ty, type_source))
        })
    })
}

fn unshadowed_undefined(node: Node<'_>, source: &[u8]) -> bool {
    matches!(node.kind(), "identifier" | "undefined")
        && node.utf8_text(source).ok() == Some("undefined")
        && crate::commands::api_changes::ts_const_arg::collect_bindings_named(
            module_root(node),
            source,
            "undefined",
        )
        .is_empty()
}
